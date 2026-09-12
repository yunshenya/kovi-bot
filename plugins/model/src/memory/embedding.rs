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
