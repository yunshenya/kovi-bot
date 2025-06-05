use sysinfo::{ProcessExt, System, SystemExt};
use systemstat::Platform;


fn format_uptime(seconds: u64) -> String{
    let days = seconds / 86400;        // 天：86400秒 = 24*60*60
    let hours = (seconds % 86400) / 3600; // 小时：剩余秒数转小时
    let minutes = (seconds % 3600) / 60;   // 分钟：剩余秒数转分钟
    format!("{}天 {}小时 {}分钟", days, hours, minutes)
}

pub fn system_info_get() -> (String, String) {
    // 初始化系统信息
    let mut system = System::new_all();
    system.refresh_all();  // 刷新数据

    let sys = systemstat::System::new();
    let update_time = format_uptime(sys.uptime().unwrap().as_secs());

    let mut process_now = String::new();
    // 获取当前进程的内存占用（单位：字节）
    let pid = sysinfo::get_current_pid().expect("获取进程ID失败");
    if let Some(process) = system.process(pid) {
        process_now = format!("内存占用: {} MB",( process.memory() / 1024) / 1024);
    };
    (update_time, process_now)
}
