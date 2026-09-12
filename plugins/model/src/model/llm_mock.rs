//! 测试专用的模型替身（Hindsight 的 MockLLM 对应物）。
//!
//! 为什么需要它：这条会话里查出的四条"管道在、但静默不干活"，全都发生在**接缝**上——
//! belief 协议没进提示词、深度反思从未触发、群聊缺一整类命令分支、立场闸门槛太高。
//! 单元测试结构上测不到它们，因为**每一条都跨过了模型调用**，而模型调用是全局函数，
//! 没有替身就只能靠真机。
//!
//! Hindsight 的经验写得很直白：他们给用户可见的能力配"黑盒故事"，真跑一遍管道，
//! 因为"~500 个测试文件结构上覆盖不到的就是接缝——consolidation 把它脚下的证据擦掉、
//! 或者一次 transfer 丢掉了 evidence"。
//!
//! 用法（测试里）：
//! ```ignore
//! with_mock_model("returns a stance", |_| r#"[{"kind":"form","proposition":"我认为慢一点更好"}]"#.to_string(),
//!     || async { runtime.form_stances(&input).await }).await;
//! ```
//!
//! 替身是**进程级**的，而测试并行跑，所以 [`with_mock_model`] 持一把全局锁把
//! "装替身 → 跑管道 → 拆替身"串起来；用了它的测试之间自动互斥。

use serde_json::Value;
use std::sync::{LazyLock, Mutex};

type Responder = Box<dyn Fn(&Value) -> String + Send + Sync + 'static>;

static RESPONDER: LazyLock<Mutex<Option<Responder>>> = LazyLock::new(|| Mutex::new(None));

/// 测试之间串行化用。持锁期间其它 `with_mock_model` 会等。
static SERIAL: LazyLock<kovi::tokio::sync::Mutex<()>> =
    LazyLock::new(|| kovi::tokio::sync::Mutex::new(()));

/// 装一个替身并跑 `body`；跑完无论成败都拆掉，避免污染别的测试。
///
/// `label` 只用于失败时的可读性（会打印出来，便于定位是谁把替身留在了场上）。
pub(crate) async fn with_mock_model<F, T>(
    label: &str,
    responder: impl Fn(&Value) -> String + Send + Sync + 'static,
    body: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let _serial = SERIAL.lock().await;
    {
        let mut slot = RESPONDER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            slot.is_none(),
            "上一个模型替身没拆干净（{label}）——替身必须成对装卸"
        );
        *slot = Some(Box::new(responder));
    }
    let result = body.await;
    {
        let mut slot = RESPONDER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = None;
    }
    result
}

/// 当前是否装了替身。用于"替身在场时不必要求 API 密钥"——请求根本不出网。
pub(crate) fn is_installed() -> bool {
    RESPONDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some()
}

/// 取一次替身回复；没装替身就返回 `None`，调用方走真实 HTTP。
pub(crate) fn take_response(request_body: &Value) -> Option<String> {
    let slot = RESPONDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.as_ref().map(|responder| responder(request_body))
}
