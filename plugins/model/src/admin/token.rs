//! 管理后台的登录 Token：从哪里来、怎么生成、怎么落盘。

use crate::config::AdminConfig;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use uuid::Uuid;

/// 自动生成的 Token 文件名。
pub(crate) const TOKEN_FILE: &str = ".yunxi-admin-token";

/// Token 落盘位置。
///
/// 放在运行时目录里而不是工作目录：生产部署的 `current/` 是只读发布目录，
/// 写不进去；`KOVI_READY_FILE` 所在目录才是这台机器上可写且能跨发布保留的位置。
pub(crate) fn token_path() -> PathBuf {
    crate::config::runtime_dir().join(TOKEN_FILE)
}

/// Token 是否来自环境变量。只影响启动日志说什么，不参与鉴权。
static FROM_ENVIRONMENT: OnceLock<bool> = OnceLock::new();

pub(crate) fn came_from_environment() -> bool {
    *FROM_ENVIRONMENT.get().unwrap_or(&false)
}

fn mark_source(from_environment: bool) {
    let _ = FROM_ENVIRONMENT.set(from_environment);
}

/// 解析登录 Token。
///
/// 优先级：配置文件里的 `admin.token` > `admin.token_env` 指向的环境变量 >
/// 工作目录的 `.yunxi-admin-token` > 现场生成一个并落盘。
///
/// 最后那一步是刻意的：管理后台默认开启且必须登录，如果没有任何 Token 来源就
/// 直接拒绝服务，运维会被卡在"要登录先配 Token、要配 Token 先登录"的鸡生蛋里。
/// 生成一次、落盘、打印，既保证默认可用，也保证重启后 Token 不变。
pub(crate) fn resolve(config: &AdminConfig) -> anyhow::Result<String> {
    let explicit = config.token().trim();
    if !explicit.is_empty() {
        mark_source(false);
        return Ok(explicit.to_string());
    }

    let env_name = config.token_env().trim();
    if !env_name.is_empty()
        && let Ok(value) = std::env::var(env_name)
    {
        let value = value.trim();
        if !value.is_empty() {
            mark_source(true);
            return Ok(value.to_string());
        }
    }

    let path = token_path();
    if let Ok(text) = fs::read_to_string(&path) {
        let existing = text.trim();
        if !existing.is_empty() {
            mark_source(false);
            return Ok(existing.to_string());
        }
    }

    let token = generate_token();
    write_token_file(&path, &token)?;
    mark_source(false);
    println!("┌───────────────────────────────────────────────────────────────");
    println!("│ 芸汐管理后台首次启动，已生成登录 Token：");
    println!("│   {token}");
    println!("│ 已写入 {}（权限 600），重启后继续使用。", path.display());
    if !env_name.is_empty() {
        println!("│ 也可以设置环境变量 {env_name} 覆盖它。");
    }
    println!("└───────────────────────────────────────────────────────────────");
    Ok(token)
}

/// 生成 256 位随机 Token（两个 v4 UUID 拼接，来源是操作系统的 CSPRNG）。
fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn write_token_file(path: &Path, token: &str) -> anyhow::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| anyhow::anyhow!("无法写入 {}: {error}", path.display()))?;
    file.write_all(token.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| anyhow::anyhow!("无法写入 {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::generate_token;

    #[test]
    fn generated_tokens_are_long_and_unique() {
        let first = generate_token();
        let second = generate_token();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
