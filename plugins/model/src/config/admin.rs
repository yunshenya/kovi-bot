//! # 管理后台配置模块
//!
//! 芸汐自带一个 Web 管理后台（配置 + 记忆），由本进程内的 axum 服务提供。
//! 这里只描述它的监听地址、登录 Token 来源和会话时长。

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Component, Path};

/// 管理后台配置结构体
///
/// 默认只监听回环地址并要求 Token 登录：后台能改全部配置、能读全部记忆，
/// 等同于机器人本身的控制面，因此默认不接受远程连接。确实需要跨主机访问
/// 时应显式打开 `allow_non_loopback`，并且只暴露在受控私网 / VPN / 反向代理
/// 之后。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct AdminConfig {
    /// 是否启用自带管理后台。关闭后不监听任何端口。
    enabled: bool,
    /// 监听地址。默认仅本机；非回环地址需要同时打开 allow_non_loopback。
    host: String,
    /// 监听端口。刻意避开 NapCat WebUI 的 6099。
    port: u16,
    /// 登录 Token 所在的环境变量名。为空时回退到 token 字段。
    token_env: String,
    /// 可选的登录 Token 明文。留空时用 token_env；两者都为空则自动生成并
    /// 落盘到工作目录的 .yunxi-admin-token（权限 600）后打印一次。
    token: String,
    /// 登录会话有效期（秒）。默认 12 小时。
    session_ttl_secs: u64,
    /// 显式允许监听非回环地址。默认关闭，防止把控制面误暴露到公网。
    allow_non_loopback: bool,
    /// 数据标注批次目录。相对路径以运行时目录为基准（生产部署里只有运行时
    /// 目录可写），填绝对路径则以绝对路径为准。
    annotation_dir: String,
}

impl AdminConfig {
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn host(&self) -> &str {
        self.host.as_str()
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    #[must_use]
    pub fn token_env(&self) -> &str {
        self.token_env.as_str()
    }

    #[must_use]
    pub fn token(&self) -> &str {
        self.token.as_str()
    }

    #[must_use]
    pub const fn session_ttl_secs(&self) -> u64 {
        self.session_ttl_secs
    }

    #[must_use]
    pub const fn allow_non_loopback(&self) -> bool {
        self.allow_non_loopback
    }

    #[must_use]
    pub fn annotation_dir(&self) -> &str {
        self.annotation_dir.as_str()
    }

    /// 监听地址是否是本机回环（含 localhost 与 127.0.0.0/8、::1）。
    #[must_use]
    pub fn binds_loopback_only(&self) -> bool {
        is_loopback_host(&self.host)
    }

    /// 验证管理后台配置。
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.port != 0,
            "admin.port 必须是 1-65535；写 0 会让后台随机监听一个端口，运维将无法预期访问地址"
        );
        if !self.enabled {
            return Ok(());
        }
        ensure!(!self.host.trim().is_empty(), "admin.host 不能为空");
        // 标注目录是后台唯一会往里写业务数据的地方：只允许"目录名或绝对路径"，
        // 不接受 `..`，免得一次配置手滑把批次文件写到目录之外。
        let annotation_dir = self.annotation_dir.trim();
        ensure!(
            !annotation_dir.is_empty(),
            "admin.annotation_dir 不能为空：标注页需要一个可读写的批次目录"
        );
        ensure!(
            annotation_dir.len() <= 512,
            "admin.annotation_dir 过长（上限 512 字节）"
        );
        ensure!(
            !Path::new(annotation_dir)
                .components()
                .any(|part| matches!(part, Component::ParentDir)),
            "admin.annotation_dir 不能包含 `..`：标注页只读写它自己那一层目录"
        );
        ensure!(
            self.session_ttl_secs >= 300,
            "admin.session_ttl_secs 不能小于 300 秒"
        );
        if !self.binds_loopback_only() && !self.allow_non_loopback {
            return Err(anyhow::anyhow!(
                "admin.host 是非回环地址 {host}：后台可以修改全部配置并读取全部记忆，\
                 默认拒绝监听。确认只在受控网络里暴露后，显式设置 admin.allow_non_loopback = true",
                host = self.host
            ));
        }
        Ok(())
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".to_string(),
            // NapCat WebUI 占用 6099；挨着放但不要撞车。
            port: 6098,
            token_env: "YUNXI_ADMIN_TOKEN".to_string(),
            token: String::new(),
            session_ttl_secs: 12 * 60 * 60,
            allow_non_loopback: false,
            // 相对运行时目录：生产里 current/ 是只读发布目录，只有 runtime/ 可写。
            annotation_dir: super::DEFAULT_ANNOTATION_DIR.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AdminConfig;
    use std::path::Path;

    #[test]
    fn default_binds_loopback_with_the_reserved_admin_port() {
        let config = AdminConfig::default();
        assert!(config.enabled());
        assert_eq!(config.host(), "127.0.0.1");
        assert_eq!(config.port(), 6098);
        assert_eq!(config.token_env(), "YUNXI_ADMIN_TOKEN");
        assert!(config.binds_loopback_only());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn non_loopback_binding_requires_an_explicit_opt_in() {
        let remote = AdminConfig {
            host: "0.0.0.0".to_string(),
            ..AdminConfig::default()
        };
        assert!(remote.validate().is_err());

        let allowed = AdminConfig {
            allow_non_loopback: true,
            ..remote
        };
        assert!(allowed.validate().is_ok());
    }

    #[test]
    fn disabled_admin_still_rejects_an_impossible_port() {
        let disabled = AdminConfig {
            enabled: false,
            port: 0,
            ..AdminConfig::default()
        };
        assert!(disabled.validate().is_err());
    }

    #[test]
    fn ipv6_loopback_counts_as_local() {
        let local = AdminConfig {
            host: "::1".to_string(),
            ..AdminConfig::default()
        };
        assert!(local.binds_loopback_only());
        assert!(local.validate().is_ok());
    }

    #[test]
    fn annotation_dir_defaults_to_a_runtime_relative_directory() {
        let config = AdminConfig::default();
        assert_eq!(config.annotation_dir(), "turngate");
        assert!(!Path::new(config.annotation_dir()).is_absolute());
    }

    #[test]
    fn annotation_dir_rejects_escaping_the_configured_directory() {
        let escaping = AdminConfig {
            annotation_dir: "../elsewhere".to_string(),
            ..AdminConfig::default()
        };
        assert!(escaping.validate().is_err());

        let empty = AdminConfig {
            annotation_dir: "   ".to_string(),
            ..AdminConfig::default()
        };
        assert!(empty.validate().is_err());

        let absolute = AdminConfig {
            annotation_dir: "/var/lib/kovi/turngate".to_string(),
            ..AdminConfig::default()
        };
        assert!(absolute.validate().is_ok());
    }
}
