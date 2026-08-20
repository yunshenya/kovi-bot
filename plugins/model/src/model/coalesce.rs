use kovi::tokio::sync::Mutex;
use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

const COALESCE_DELAY: Duration = Duration::from_millis(850);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TextBatch {
    pub(crate) text: String,
    pub(crate) addressed: bool,
}

struct PendingBatch {
    generation: u64,
    parts: Vec<String>,
    addressed: bool,
    updated_at: Instant,
}

impl Default for PendingBatch {
    fn default() -> Self {
        Self {
            generation: 0,
            parts: Vec::new(),
            addressed: false,
            updated_at: Instant::now(),
        }
    }
}

/// 为每个聊天键提供轻量防抖队列；只有最后到达的任务会取走完整批次。
pub(crate) struct MessageCoalescer<K> {
    pending: Mutex<HashMap<K, PendingBatch>>,
}

impl<K> Default for MessageCoalescer<K> {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl<K> MessageCoalescer<K>
where
    K: Copy + Eq + Hash,
{
    pub(crate) async fn push(&self, key: K, message: String, addressed: bool) -> Option<TextBatch> {
        let generation = {
            let mut pending = self.pending.lock().await;
            if pending.len() > 2_048 {
                pending.retain(|_, batch| batch.updated_at.elapsed() < Duration::from_secs(10));
            }
            let batch = pending.entry(key).or_default();
            batch.generation = batch.generation.wrapping_add(1);
            batch.parts.push(message);
            batch.addressed |= addressed;
            batch.updated_at = Instant::now();
            batch.generation
        };
        kovi::tokio::time::sleep(COALESCE_DELAY).await;

        let mut pending = self.pending.lock().await;
        if pending.get(&key)?.generation != generation {
            return None;
        }
        pending.remove(&key).map(|batch| TextBatch {
            text: batch.parts.join("\n"),
            addressed: batch.addressed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{MessageCoalescer, TextBatch};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn rapid_messages_are_returned_as_one_batch() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let coalescer = Arc::new(MessageCoalescer::default());
                let first = {
                    let coalescer = Arc::clone(&coalescer);
                    kovi::tokio::spawn(async move {
                        coalescer.push(7_i64, "第一句".to_string(), true).await
                    })
                };
                kovi::tokio::time::sleep(Duration::from_millis(20)).await;
                let second = coalescer.push(7_i64, "第二句".to_string(), false).await;

                assert!(first.await.expect("任务应正常结束").is_none());
                assert_eq!(
                    second,
                    Some(TextBatch {
                        text: "第一句\n第二句".to_string(),
                        addressed: true,
                    })
                );
            });
    }
}
