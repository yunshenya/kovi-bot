//! 嵌入服务客户端：把文本交给本机的 `yunxi-embed`，换回向量。
//!
//! 服务在 `tools/embed-service/`，只监听回环地址。这一层刻意做得**很薄且可失败**：
//! 服务没起来、超时、返回异常，一律返回 `Err`，调用方据此退回纯词面检索。
//! 也就是说——**嵌入服务挂了，她的记忆检索退化到今天的水平，而不是整个失灵**。
//!
//! 这条"退化成可用"的要求不是客套：一个新的外部依赖如果能让主链路挂掉，那它
//! 迟早会。检索是每轮对话都要走的路，不能赌一个 sidecar 永远健康。

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;

use crate::config;

/// 单次请求最多带几段文本。与服务端的 MAX_BATCH 保持一致，超过就分批。
const MAX_BATCH: usize = 64;
/// 单段最多多少字符。服务端也会截断，这里先截一次省流量。
const MAX_CHARS: usize = 1_024;

static EMBED_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_else(|error| {
            eprintln!("[WARN] 嵌入客户端构建失败，改用默认配置: {error}");
            reqwest::Client::new()
        })
});

pub(crate) struct EmbeddingClient {
    endpoint: String,
    model: String,
    timeout: Duration,
}

impl EmbeddingClient {
    /// 从配置建客户端；未启用时返回 `None`（调用方直接走词面那一路）。
    pub(crate) fn from_config() -> Option<Self> {
        let memory = config::get().memory().clone();
        if !memory.embedding_enabled() {
            return None;
        }
        let endpoint = memory
            .embedding_url()
            .trim()
            .trim_end_matches('/')
            .to_string();
        if endpoint.is_empty() {
            return None;
        }
        Some(Self {
            endpoint,
            model: memory.embedding_model().to_string(),
            timeout: Duration::from_secs(memory.embedding_timeout_secs()),
        })
    }

    /// 指定端点的构造，供测试用进程内假服务替掉真实 sidecar。
    #[cfg(test)]
    pub(crate) fn with_endpoint(endpoint: &str, model: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            timeout: Duration::from_secs(5),
        }
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// 编码一批文本。`query=true` 时服务端会加 bge 的查询侧指令前缀。
    ///
    /// 分批：服务端单请求上限 64 段，超了就切开，免得一个大上下文把请求顶回去。
    pub(crate) async fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(MAX_BATCH) {
            let prepared: Vec<String> = batch
                .iter()
                .map(|text| text.chars().take(MAX_CHARS).collect())
                .collect();
            let body = json!({"texts": prepared, "query": query});
            let response = EMBED_CLIENT
                .post(format!("{}/v1/embed", self.endpoint))
                .timeout(self.timeout)
                .json(&body)
                .send()
                .await
                .map_err(|error| anyhow!("嵌入服务请求失败: {error}"))?;
            if !response.status().is_success() {
                return Err(anyhow!("嵌入服务返回 {}", response.status()));
            }
            let payload: Value = response
                .json()
                .await
                .map_err(|error| anyhow!("嵌入服务响应无法解析: {error}"))?;
            let batch_vectors = payload
                .get("vectors")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("嵌入服务响应缺少 vectors"))?;
            if batch_vectors.len() != batch.len() {
                return Err(anyhow!(
                    "嵌入服务返回 {} 条向量，请求了 {} 段",
                    batch_vectors.len(),
                    batch.len()
                ));
            }
            for vector in batch_vectors {
                let values = vector
                    .as_array()
                    .ok_or_else(|| anyhow!("向量不是数组"))?
                    .iter()
                    .map(|value| value.as_f64().unwrap_or(0.0) as f32)
                    .collect::<Vec<f32>>();
                if values.is_empty() {
                    return Err(anyhow!("嵌入服务返回了空向量"));
                }
                vectors.push(values);
            }
        }
        Ok(vectors)
    }
}

impl EmbeddingClient {
    /// 交叉编码重排：把 (查询, 文档) 成对送进模型打分，返回**按分数降序的原始下标**。
    ///
    /// 与嵌入的区别：嵌入是各编各的再算距离，重排是两段文本一起过模型，所以更准也更慢。
    /// 实测它纠正过嵌入的真实错误：查询"我喜欢安静的地方"时，嵌入把"她喜欢看书"排在
    /// 真正相关的"他讨厌吵闹的环境"前面——因为共用了一个"喜欢"。重排的分数也分得开
    /// 得多（0.276 / 0.041 / 0.0002，而余弦挤在 0.36–0.52）。
    pub(crate) async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<usize>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let prepared: Vec<String> = documents
            .iter()
            .map(|text| text.chars().take(MAX_CHARS).collect())
            .collect();
        let body = json!({"query": query, "documents": prepared});
        let response = EMBED_CLIENT
            .post(format!("{}/v1/rerank", self.endpoint))
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| anyhow!("重排服务请求失败: {error}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("重排服务返回 {}", response.status()));
        }
        let payload: Value = response
            .json()
            .await
            .map_err(|error| anyhow!("重排响应无法解析: {error}"))?;
        let results = payload
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("重排响应缺少 results"))?;
        let mut ranked = Vec::with_capacity(results.len());
        for item in results {
            let index = item
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("重排结果缺少 index"))? as usize;
            if index < documents.len() {
                ranked.push(index);
            }
        }
        if ranked.len() != documents.len() {
            // 少一条就意味着有文档没被打分——宁可整批不信，也不要按半份结果重排。
            return Err(anyhow!(
                "重排只返回了 {} 条，请求了 {} 篇",
                ranked.len(),
                documents.len()
            ));
        }
        Ok(ranked)
    }
}

/// 余弦相似度。服务端已做 L2 归一化，所以这里其实就是点积——但仍然做完整计算，
/// 免得哪天换了不做归一化的模型就悄悄算错。
pub(crate) fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

/// 向量的字节形式：小端 f32 连排。存 BYTEA 而不存文本数组，省空间也省解析。
pub(crate) fn vector_to_bytes(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub(crate) fn vector_from_bytes(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 进程内假服务：**接缝要测在边界上**。
    ///
    /// 今天所有真 bug 都出在接缝（协议没进提示词、深反从未触发、缺分支、闸门槛、
    /// belief_id 漏填），而"Rust 客户端 ↔ 嵌入服务"这条 HTTP 边界同样是接缝——
    /// 真机上它只在部署后才被踩到。这里用一个最小 HTTP 服务把它钉住：
    /// 响应格式、分批、条数对不上、503 降级，全都不用等真机。
    fn spawn_fake_service(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0_u8; 8192];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{address}"), handle)
    }

    fn runtime() -> kovi::tokio::runtime::Runtime {
        kovi::tokio::runtime::Runtime::new().expect("test runtime")
    }

    #[test]
    fn embed_client_parses_a_well_formed_service_response() {
        let (endpoint, handle) = spawn_fake_service(vec![(
            200,
            r#"{"vectors":[[0.1,0.2],[0.3,0.4]],"dim":2}"#.into(),
        )]);
        runtime().block_on(async {
            let client = EmbeddingClient::with_endpoint(&endpoint, "fake");
            let vectors = client
                .embed(&["一".to_string(), "二".to_string()], false)
                .await
                .expect("应能解析");
            assert_eq!(vectors.len(), 2);
            assert!((vectors[0][0] - 0.1).abs() < 1e-6);
        });
        let _ = handle.join();
    }

    #[test]
    fn embed_client_rejects_a_short_response_instead_of_misaligning() {
        // 返回条数少于请求条数是最危险的一种"成功"：如果照单全收，后面按位置
        // 对齐的向量会整体错位，而没有任何报错。必须当失败处理。
        let (endpoint, handle) =
            spawn_fake_service(vec![(200, r#"{"vectors":[[0.1,0.2]]}"#.into())]);
        runtime().block_on(async {
            let client = EmbeddingClient::with_endpoint(&endpoint, "fake");
            let error = client
                .embed(&["一".to_string(), "二".to_string()], false)
                .await
                .expect_err("条数对不上应当报错");
            assert!(error.to_string().contains("请求了 2 段"), "实际: {error}");
        });
        let _ = handle.join();
    }

    #[test]
    fn rerank_client_falls_back_when_the_service_lacks_a_reranker() {
        // 503 = 没装重排器。调用方据此保持融合顺序，绝不能因此让检索失败。
        let (endpoint, handle) =
            spawn_fake_service(vec![(503, r#"{"ok":false,"error":"重排器未安装"}"#.into())]);
        runtime().block_on(async {
            let client = EmbeddingClient::with_endpoint(&endpoint, "fake");
            assert!(client.rerank("查询", &["甲".to_string()]).await.is_err());
        });
        let _ = handle.join();
    }

    #[test]
    fn rerank_client_returns_the_service_order_as_indices() {
        let (endpoint, handle) = spawn_fake_service(vec![(
            200,
            r#"{"results":[{"index":2,"score":0.9},{"index":0,"score":0.4},{"index":1,"score":0.1}]}"#
                .into(),
        )]);
        runtime().block_on(async {
            let client = EmbeddingClient::with_endpoint(&endpoint, "fake");
            let ranked = client
                .rerank(
                    "查询",
                    &["甲".to_string(), "乙".to_string(), "丙".to_string()],
                )
                .await
                .expect("应能解析");
            assert_eq!(ranked, vec![2, 0, 1], "必须原样返回服务给的顺序");
        });
        let _ = handle.join();
    }

    #[test]
    fn cosine_matches_hand_computed_values() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        // 长度不匹配或全零：老实返回 0，不 panic、不瞎算。
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn vectors_round_trip_through_bytes() {
        let vector = vec![0.5_f32, -1.25, 3.0, 0.0];
        let bytes = vector_to_bytes(&vector);
        assert_eq!(bytes.len(), vector.len() * 4);
        assert_eq!(vector_from_bytes(&bytes), Some(vector));
        // 坏输入不能 panic——它来自数据库，不该让检索整条挂掉。
        assert_eq!(vector_from_bytes(&[]), None);
        assert_eq!(vector_from_bytes(&[1, 2, 3]), None);
    }
}
