//! 本机与芸汐进程的一次性快照。
//!
//! 两件事必须分清，线上就踩过这个坑：`System::uptime()` 是**机器开机时长**，
//! 不是机器人跑了多久。后台概览把它当成"系统运行"、又和芸汐的 pid 并排显示，
//! 于是刚发完版的人看到"系统运行 6 天"，会以为新版本根本没起来。所以这里一次
//! 刷新同时给出两者，调用方各写各的标签。

use sysinfo::System;

/// 本机与进程的现状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSnapshot {
    /// 机器开机时长（给人看的写法）。
    pub host_uptime: String,
    /// 芸汐进程已运行多少秒（读不到为 `None`）。
    pub process_uptime_secs: Option<u64>,
    /// 芸汐进程内存（给人看的写法）。
    pub process_memory: String,
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400; // 天：86400秒 = 24*60*60
    let hours = (seconds % 86400) / 3600; // 小时：剩余秒数转小时
    let minutes = (seconds % 3600) / 60; // 分钟：剩余秒数转分钟
    format!("{}天 {}小时 {}分钟", days, hours, minutes)
}

/// 采一次快照。`System::new_all()` 会刷新整机信息，所以一次采样尽量把要用的都取走。
pub fn system_snapshot() -> SystemSnapshot {
    let mut system = System::new_all();
    system.refresh_all(); // 刷新数据

    let mut process_uptime_secs = None;
    let mut process_memory = String::from("芸汐进程内存: 获取失败");
    if let Ok(pid) = sysinfo::get_current_pid()
        && let Some(process) = system.process(pid)
    {
        process_uptime_secs = Some(process.run_time());
        process_memory = format!("芸汐进程内存: {} MB", (process.memory() / 1024) / 1024);
    }
    SystemSnapshot {
        host_uptime: format_uptime(System::uptime()),
        process_uptime_secs,
        process_memory,
    }
}

/// 兼容老接口：`#系统信息` 那条命令要的就是"开机时长 + 进程内存"两行字。
pub fn system_info_get() -> (String, String) {
    let snapshot = system_snapshot();
    (snapshot.host_uptime, snapshot.process_memory)
}

/// 进程运行时长给人看的写法（"5 分钟" / "2 小时 13 分钟" / "6 天 1 小时"）。
///
/// 与 [`format_uptime`] 分开：那个是主机的三位写法（天/小时/分钟），这个给进程用，
/// 刚发完版时最有意义的单位是分钟，写成"0天 0小时 5分钟"反而不好读。
pub fn format_process_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        return format!("{days} 天 {hours} 小时");
    }
    if hours > 0 {
        return format!("{hours} 小时 {minutes} 分钟");
    }
    if minutes > 0 {
        return format!("{minutes} 分钟");
    }
    format!("{seconds} 秒")
}

#[cfg(test)]
mod tests {
    use super::{SystemSnapshot, format_process_uptime, system_snapshot};

    /// 进程时长要按量级换单位：刚重启时"5 分钟"比"0天 0小时 5分钟"有用得多。
    #[test]
    fn process_uptime_picks_a_readable_unit() {
        assert_eq!(format_process_uptime(5), "5 秒");
        assert_eq!(format_process_uptime(300), "5 分钟");
        assert_eq!(format_process_uptime(3_600 + 13 * 60), "1 小时 13 分钟");
        assert_eq!(format_process_uptime(6 * 86_400 + 3_600), "6 天 1 小时");
    }

    /// 快照本身：本机一定开着机、芸汐进程一定在跑，所以两个数都该拿得到。
    #[test]
    fn snapshot_reads_both_the_host_and_this_process() {
        let snapshot = system_snapshot();
        assert!(
            snapshot.host_uptime.contains('天'),
            "{}",
            snapshot.host_uptime
        );
        let uptime = snapshot
            .process_uptime_secs
            .expect("测试进程自己一定读得到运行时长");
        assert!(uptime < 86_400, "测试进程不该已经跑了一天：{uptime}");
        assert!(
            snapshot.process_memory.contains("芸汐进程内存"),
            "{}",
            snapshot.process_memory
        );
    }

    #[test]
    fn snapshot_stays_comparable() {
        // 值会变，但结构稳定：拿两次也只是为了钉住"同一个进程拿到的是同一类数据"。
        let first: SystemSnapshot = system_snapshot();
        let second = system_snapshot();
        assert_eq!(
            first.process_memory.is_empty(),
            second.process_memory.is_empty()
        );
    }
}
