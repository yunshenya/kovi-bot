//! TurnGate 完成度分类器宿主运行时 (Phase 2 接线, doc §8.2/§9)。
//!
//! `[model.turn_gate]` (disabled/shadow/active) 控制参与方式:
//! - active:TurnGate 优先决定 flush/hold;请先看 bootstrap 日志确认已加载
//!   校验后的 bundle;abstain 或引擎不可用时回退现有 lexical + MiniMind 路径;
//! - shadow:路由保持现有路径,但记录 TurnGate 决策与现有路径的分歧
//!   (Phase 3 上 response head 前需要这些指标);
//! - disabled:完全不参与。
//!
//! 加载失败绝不阻断 Core (doc §6):引擎保持不可用,completion 回退现有
//! 路径;bundle 变更后重启进程即可重新加载。

use crate::config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use yunxi_core::{InputCompletion, TurnCompletion, TurnGateEngine, TurnGateInput, TurnGateMetrics};

static HOST_RUNTIME: OnceLock<Arc<TurnGateHostRuntime>> = OnceLock::new();

/// 供测试/校准直接调用:TurnGate 决策 → coalescer 的 InputCompletion 语义。
#[must_use]
pub(crate) const fn map_completion(decision: TurnCompletion) -> InputCompletion {
    match decision {
        TurnCompletion::FlushNow => InputCompletion::Complete,
        TurnCompletion::HoldForMore => InputCompletion::Incomplete,
        // Abstain 不是类别;调用方必须先走回退路径,这里永远不应到达。
        TurnCompletion::Abstain => InputCompletion::Incomplete,
    }
}

pub(crate) struct TurnGateHostRuntime {
    engine: StdMutex<Option<Arc<TurnGateEngine>>>,
    metrics: TurnGateMetrics,
    legacy_complete: AtomicU64,
    legacy_incomplete: AtomicU64,
    split: AtomicU64,
    mode_active: bool,
    mode_shadow: bool,
    asset_dir: PathBuf,
}

impl TurnGateHostRuntime {
    /// 用 `[model.turn_gate]` 配置加载 bundle;失败 fail-soft,仅记录日志。
    pub(crate) fn install() -> Arc<Self> {
        let turn_gate_cfg = config::get().model().turn_gate().clone();
        let runtime = Arc::new(Self {
            engine: StdMutex::new(None),
            metrics: TurnGateMetrics::default(),
            legacy_complete: AtomicU64::new(0),
            legacy_incomplete: AtomicU64::new(0),
            split: AtomicU64::new(0),
            mode_active: turn_gate_cfg.enabled() && turn_gate_cfg.mode() == "active",
            mode_shadow: turn_gate_cfg.enabled() && turn_gate_cfg.mode() == "shadow",
            asset_dir: PathBuf::from(turn_gate_cfg.asset_dir()),
        });
        runtime.load_engine();
        kovi::log::info!(
            "[TURNGATE] completion gate: mode={} engine_available={}",
            turn_gate_cfg.mode(),
            runtime.engine_available(),
        );
        HOST_RUNTIME.set(Arc::clone(&runtime)).unwrap_or(());
        HOST_RUNTIME.get().cloned().unwrap_or(runtime)
    }

    /// 线程安全地加载/重载权重,返回是否成功 (失败保持旧引擎,见 doc §6)。
    pub(crate) fn load_engine(&self) -> bool {
        match TurnGateEngine::load_from_path(&self.asset_dir) {
            Ok(engine) => {
                kovi::log::info!(
                    "[TURNGATE] bundle 已加载: model_version={} feature_version={} dir={:?}",
                    engine.model_version(),
                    engine.feature_signature(),
                    self.asset_dir,
                );
                *self.engine.lock().expect("turn gate engine lock") = Some(Arc::new(engine));
                true
            }
            Err(error) => {
                kovi::log::info!("[TURNGATE] bundle 不可用 (回退 lexical+MiniMind): {error}");
                false
            }
        }
    }

    pub(crate) fn engine_available(&self) -> bool {
        self.engine
            .lock()
            .expect("turn gate engine lock")
            .as_ref()
            .is_some_and(|engine| engine.available())
    }

    /// 决策线 (doc §8.2 第 4 条):TurnGate 有高置信度决策时优先,否则调用
    /// `legacy` (现有 lexical + MiniMind 路径)。shadow 模式下路由始终走
    /// legacy,但记录 TurnGate 决策与分歧。
    pub(crate) async fn classify_completion<F, Fut>(
        &self,
        input: &TurnGateInput,
        legacy: F,
    ) -> InputCompletion
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = InputCompletion>,
    {
        self.metrics.record_request();
        let engine = self.engine.lock().expect("turn gate engine lock").clone();
        let turn_gate_decision = engine
            .as_ref()
            .map(|engine| engine.classify_completion(input));
        let decide = matches!(
            turn_gate_decision,
            Some(ref output) if output.decision != TurnCompletion::Abstain
        );

        if !self.mode_active || !decide {
            let legacy_decision = legacy().await;
            self.record_legacy(&legacy_decision);
            if let Some(output) = turn_gate_decision.as_ref() {
                self.metrics.record_completion(output);
                let turned = map_completion(output.decision);
                if turned != legacy_decision {
                    self.split.fetch_add(1, Ordering::Relaxed);
                    kovi::log::debug!(
                        "Yunxi TurnGate completion split: gate={:?} legacy={:?} mode={}",
                        output.decision,
                        legacy_decision,
                        if self.mode_shadow {
                            "shadow"
                        } else {
                            "fallback"
                        },
                    );
                }
            } else {
                kovi::log::trace!("Yunxi TurnGate engine unavailable, using legacy completion");
            }
            return legacy_decision;
        }

        let output = turn_gate_decision.expect("decide requires a TurnGate output");
        self.metrics.record_completion(&output);
        let completion = map_completion(output.decision);
        kovi::log::debug!(
            "Yunxi TurnGate completion: decision={:?} confidence={:.3}",
            output.decision,
            output.confidence,
        );
        completion
    }

    fn record_legacy(&self, completion: &InputCompletion) {
        match completion {
            InputCompletion::Complete => {
                self.legacy_complete.fetch_add(1, Ordering::Relaxed);
            }
            InputCompletion::Incomplete => {
                self.legacy_incomplete.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[cfg(test)]
    fn deployment_healthy(&self) -> bool {
        self.engine_available() && self.mode_active
    }
}

/// 供测试验证部署健康语义(engine 可用且 active 才算真正接管)。
/// 安装并加载 TurnGate bundle (bridge 启动时调用一次)。
pub(crate) fn install() -> Arc<TurnGateHostRuntime> {
    TurnGateHostRuntime::install()
}

pub(crate) fn get() -> Option<Arc<TurnGateHostRuntime>> {
    HOST_RUNTIME.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_mapping_is_lossless_for_confident_decisions() {
        assert!(matches!(
            map_completion(TurnCompletion::FlushNow),
            InputCompletion::Complete
        ));
        assert!(matches!(
            map_completion(TurnCompletion::HoldForMore),
            InputCompletion::Incomplete
        ));
    }

    #[test]
    fn load_engine_fail_soft_when_dir_missing() {
        // 构造一个仅缺 bundle 的运行时:load 返回 false,引擎不可用。
        let runtime = TurnGateHostRuntime {
            engine: StdMutex::new(None),
            metrics: TurnGateMetrics::default(),
            legacy_complete: AtomicU64::new(0),
            legacy_incomplete: AtomicU64::new(0),
            split: AtomicU64::new(0),
            mode_active: true,
            mode_shadow: false,
            asset_dir: PathBuf::from("/definitely/not/a/bundle/dir"),
        };
        assert!(!runtime.load_engine());
        assert!(!runtime.deployment_healthy());
    }
}
