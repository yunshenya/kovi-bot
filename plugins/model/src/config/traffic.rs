//! 入站流量、排队和响应资源边界。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct TrafficConfig {
    enabled: bool,
    window_secs: u64,
    per_user_limit: usize,
    global_limit: usize,
    cooldown_secs: u64,
    max_pending_turns: usize,
    /// 群聊 waiting room 看门狗的扫描间隔（秒）。
    ///
    /// 队列只在"有在途回合"时才该非空，排空由回合收尾触发；但 Core 链路
    /// 收尾、panic、取消这些路径不会触发它，队列一旦被落在后面就会自锁
    /// （`has_queued` 曾让后续消息一直排队，而排空永远不发生）。看门狗每
    /// 这个间隔扫一遍"队列非空且会话空闲"的群补踢一次，是最后一道保险。
    window_drain_sweep_secs: u64,
    /// "排空还在、但已经多久没推进就算卡住"的阈值（秒）。
    ///
    /// 只影响管理后台的判定与措辞，不改变任何回复行为。默认 180 秒的理由：
    /// 一次群聊回合最坏要串几次模型调用（每次 30 秒超时 ×3 次重试），把阈值压到
    /// 几十秒会把"正在慢慢回"误报成卡住；放到十分钟又会让人盯着一个已经死了的
    /// 队列白等。180 秒恰好卡在两者之间。
    window_stall_secs: u64,
    max_input_chars: usize,
    max_model_response_bytes: usize,
    max_model_queue: usize,
    model_queue_timeout_secs: u64,
}

impl TrafficConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn window_secs(&self) -> u64 {
        self.window_secs
    }

    pub fn per_user_limit(&self) -> usize {
        self.per_user_limit
    }

    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    pub fn cooldown_secs(&self) -> u64 {
        self.cooldown_secs
    }

    pub fn max_pending_turns(&self) -> usize {
        self.max_pending_turns
    }

    pub fn window_drain_sweep_secs(&self) -> u64 {
        self.window_drain_sweep_secs
    }

    pub fn window_stall_secs(&self) -> u64 {
        self.window_stall_secs
    }

    pub fn max_input_chars(&self) -> usize {
        self.max_input_chars
    }

    pub fn max_model_response_bytes(&self) -> usize {
        self.max_model_response_bytes
    }

    pub fn max_model_queue(&self) -> usize {
        self.max_model_queue
    }

    pub fn model_queue_timeout_secs(&self) -> u64 {
        self.model_queue_timeout_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.window_secs == 0 || self.cooldown_secs == 0 {
            return Err(anyhow::anyhow!("流量限制窗口和冷却时间必须大于 0"));
        }
        if self.per_user_limit == 0 || self.global_limit < self.per_user_limit {
            return Err(anyhow::anyhow!(
                "traffic.global_limit 必须不小于 traffic.per_user_limit，且都大于 0"
            ));
        }
        if !(1..=128).contains(&self.max_pending_turns) {
            return Err(anyhow::anyhow!(
                "traffic.max_pending_turns 必须在 1 到 128 之间"
            ));
        }
        if !(5..=600).contains(&self.window_drain_sweep_secs) {
            return Err(anyhow::anyhow!(
                "traffic.window_drain_sweep_secs 必须在 5 到 600 之间"
            ));
        }
        if !(30..=3_600).contains(&self.window_stall_secs) {
            return Err(anyhow::anyhow!(
                "traffic.window_stall_secs 必须在 30 到 3600 之间"
            ));
        }
        if !(256..=32_000).contains(&self.max_input_chars) {
            return Err(anyhow::anyhow!(
                "traffic.max_input_chars 必须在 256 到 32000 之间"
            ));
        }
        if !(64 * 1024..=16 * 1024 * 1024).contains(&self.max_model_response_bytes) {
            return Err(anyhow::anyhow!(
                "traffic.max_model_response_bytes 必须在 64 KiB 到 16 MiB 之间"
            ));
        }
        if !(4..=1_024).contains(&self.max_model_queue) || self.model_queue_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "traffic.max_model_queue 必须在 4 到 1024 之间，队列超时必须大于 0"
            ));
        }
        Ok(())
    }
}

impl Default for TrafficConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_secs: 60,
            per_user_limit: 20,
            global_limit: 300,
            cooldown_secs: 120,
            max_pending_turns: 16,
            window_drain_sweep_secs: 30,
            window_stall_secs: 180,
            max_input_chars: 6_000,
            max_model_response_bytes: 2 * 1024 * 1024,
            max_model_queue: 64,
            model_queue_timeout_secs: 15,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TrafficConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(TrafficConfig::default().validate().is_ok());
    }

    /// 看门狗间隔是"队列卡住之后多久被救回来"的上限：太短是白扫，太长等于
    /// 群里继续沉默，所以既拒绝 0 也拒绝小时级的值。
    #[test]
    fn window_drain_sweep_interval_is_bounded() {
        let mut config = TrafficConfig::default();
        assert_eq!(config.window_drain_sweep_secs(), 30);

        config.window_drain_sweep_secs = 0;
        assert!(config.validate().is_err());
        config.window_drain_sweep_secs = 4;
        assert!(config.validate().is_err());
        config.window_drain_sweep_secs = 601;
        assert!(config.validate().is_err());
        config.window_drain_sweep_secs = 5;
        assert!(config.validate().is_ok());
        config.window_drain_sweep_secs = 600;
        assert!(config.validate().is_ok());
    }

    /// 卡住阈值只影响后台的判定与措辞：太短会把"正在慢慢回"误报成卡住，
    /// 太长等于让一个已经死了的队列在页面上装作还在跑。
    #[test]
    fn window_stall_threshold_is_bounded() {
        let mut config = TrafficConfig::default();
        assert_eq!(config.window_stall_secs(), 180);

        config.window_stall_secs = 0;
        assert!(config.validate().is_err());
        config.window_stall_secs = 29;
        assert!(config.validate().is_err());
        config.window_stall_secs = 3_601;
        assert!(config.validate().is_err());
        config.window_stall_secs = 30;
        assert!(config.validate().is_ok());
        config.window_stall_secs = 3_600;
        assert!(config.validate().is_ok());
    }
}
