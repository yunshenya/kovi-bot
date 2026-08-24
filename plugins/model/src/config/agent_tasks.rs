use serde::{Deserialize, Serialize};

/// 跨群问答任务配置。
///
/// 这类任务会在群里发出一个问题，收集一段时间的成员回复，再把汇总
/// 发回发起任务的主管理员私聊。所有上限都在这里集中约束，避免模型参数
/// 直接扩大任务的资源和数据范围。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct AgentTaskConfig {
    /// 是否启用跨群问答任务调度和回复收集。
    enabled: bool,
    /// 调度器扫描间隔（秒）。
    poll_interval_secs: u64,
    /// 单个任务最多等待群回复的分钟数。
    max_collect_minutes: u64,
    /// 用户没有指定时的默认等待分钟数。
    default_collect_minutes: u64,
    /// 单个主管理员最多同时拥有多少条未完成任务。
    max_active_per_actor: usize,
    /// 全局最多保留多少条未完成任务。
    max_active_total: usize,
    /// 单个任务最多保存多少条群成员回复。
    max_events_per_task: usize,
    /// 单条群成员回复最多保存多少字符。
    max_event_chars: usize,
    /// 私聊汇报最多发送多少字符。
    max_report_chars: usize,
    /// 报告任务的数据库租约时间（秒）。
    lease_secs: u64,
}

impl AgentTaskConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs
    }

    pub fn max_collect_minutes(&self) -> u64 {
        self.max_collect_minutes
    }

    pub fn default_collect_minutes(&self) -> u64 {
        self.default_collect_minutes
    }

    pub fn max_active_per_actor(&self) -> usize {
        self.max_active_per_actor
    }

    pub fn max_active_total(&self) -> usize {
        self.max_active_total
    }

    pub fn max_events_per_task(&self) -> usize {
        self.max_events_per_task
    }

    pub fn max_event_chars(&self) -> usize {
        self.max_event_chars
    }

    pub fn max_report_chars(&self) -> usize {
        self.max_report_chars
    }

    pub fn lease_secs(&self) -> u64 {
        self.lease_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.poll_interval_secs == 0 || self.poll_interval_secs > 60 {
            return Err(anyhow::anyhow!(
                "agent_tasks.poll_interval_secs 必须在 1 到 60 秒之间"
            ));
        }
        if self.max_collect_minutes == 0 || self.max_collect_minutes > 1_440 {
            return Err(anyhow::anyhow!(
                "agent_tasks.max_collect_minutes 必须在 1 到 1440 分钟之间"
            ));
        }
        if self.default_collect_minutes == 0
            || self.default_collect_minutes > self.max_collect_minutes
        {
            return Err(anyhow::anyhow!(
                "agent_tasks.default_collect_minutes 必须在 1 到 max_collect_minutes 之间"
            ));
        }
        if self.max_active_per_actor == 0 || self.max_active_per_actor > 100 {
            return Err(anyhow::anyhow!(
                "agent_tasks.max_active_per_actor 必须在 1 到 100 之间"
            ));
        }
        if self.max_active_total == 0 || self.max_active_total > 10_000 {
            return Err(anyhow::anyhow!(
                "agent_tasks.max_active_total 必须在 1 到 10000 之间"
            ));
        }
        if self.max_events_per_task == 0 || self.max_events_per_task > 1_000 {
            return Err(anyhow::anyhow!(
                "agent_tasks.max_events_per_task 必须在 1 到 1000 之间"
            ));
        }
        if self.max_event_chars < 20 || self.max_event_chars > 2_000 {
            return Err(anyhow::anyhow!(
                "agent_tasks.max_event_chars 必须在 20 到 2000 之间"
            ));
        }
        if self.max_report_chars < 200 || self.max_report_chars > 8_000 {
            return Err(anyhow::anyhow!(
                "agent_tasks.max_report_chars 必须在 200 到 8000 之间"
            ));
        }
        if self.lease_secs < 10 || self.lease_secs > 600 {
            return Err(anyhow::anyhow!(
                "agent_tasks.lease_secs 必须在 10 到 600 秒之间"
            ));
        }
        Ok(())
    }
}

impl Default for AgentTaskConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: 5,
            max_collect_minutes: 120,
            default_collect_minutes: 10,
            max_active_per_actor: 20,
            max_active_total: 200,
            max_events_per_task: 200,
            max_event_chars: 500,
            max_report_chars: 3_000,
            lease_secs: 180,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AgentTaskConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(AgentTaskConfig::default().validate().is_ok());
    }

    #[test]
    fn invalid_limits_are_rejected() {
        let config = AgentTaskConfig {
            max_collect_minutes: 0,
            ..AgentTaskConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
