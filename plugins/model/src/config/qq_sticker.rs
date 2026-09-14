use serde::{Deserialize, Serialize};

/// 芸汐发表情包用的自有素材库配置。
///
/// 素材只来自这里配的目录——她不会把聊天里别人的表情存下来再发出去。目录里的
/// 每个图片文件就是一张可用表情包，文件名（去掉扩展名）就是它的标签，例如
/// `无语又想笑.gif` 的标签是 `无语又想笑`；`开心-1.png`、`开心-2.png` 这类
/// 带编号的名字会归到同一个标签 `开心` 下。
///
/// 默认关闭。打开后她才能在回复里带表情包；关掉时提示词里根本不会出现这个
/// 出口，模型不知道自己有一个当下用不了的能力。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct QqStickerConfig {
    /// 是否允许芸汐发表情包。
    enabled: bool,
    /// 素材目录。相对路径以运行时目录为基准（与 `admin.annotation_dir` 一致）。
    dir: String,
    /// 最多收录多少个素材文件；超出部分按路径排序截断。
    max_files: usize,
    /// 单个素材文件的大小上限（KB）。整张图会以 base64 走 OneBot 图片段，
    /// 上限压住的是 WebSocket 单帧体积，不是磁盘。
    max_file_kb: u64,
    /// 目录重扫间隔（秒）。把新表情丢进目录后最多等这么久就能用，不必重启。
    rescan_secs: u64,
}

impl QqStickerConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    pub fn max_files(&self) -> usize {
        self.max_files
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_kb.saturating_mul(1024)
    }

    /// 单文件上限的原始单位（KB）。后台要把它显示给人看，并据此设置上传体上限。
    pub fn max_file_kb(&self) -> u64 {
        self.max_file_kb
    }

    pub fn rescan_secs(&self) -> u64 {
        self.rescan_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.dir.trim().is_empty() {
            return Err(anyhow::anyhow!("qq_sticker.dir 在启用时不能为空"));
        }
        if !(1..=2_000).contains(&self.max_files) {
            return Err(anyhow::anyhow!(
                "qq_sticker.max_files 必须在 1 到 2000 之间"
            ));
        }
        if !(16..=8_192).contains(&self.max_file_kb) {
            return Err(anyhow::anyhow!(
                "qq_sticker.max_file_kb 必须在 16 到 8192 之间（单位 KB）"
            ));
        }
        if !(1..=3_600).contains(&self.rescan_secs) {
            return Err(anyhow::anyhow!(
                "qq_sticker.rescan_secs 必须在 1 到 3600 秒之间"
            ));
        }
        Ok(())
    }
}

impl Default for QqStickerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "stickers".to_string(),
            max_files: 200,
            max_file_kb: 2_048,
            rescan_secs: 30,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::QqStickerConfig;

    #[test]
    fn defaults_are_disabled_and_valid() {
        let config = QqStickerConfig::default();
        assert!(!config.enabled());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn disabled_configuration_skips_validation() {
        let config = QqStickerConfig {
            enabled: false,
            dir: "   ".to_string(),
            ..QqStickerConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn enabled_requires_a_directory_and_sane_bounds() {
        let empty_dir = QqStickerConfig {
            enabled: true,
            dir: String::new(),
            ..QqStickerConfig::default()
        };
        assert!(empty_dir.validate().is_err());

        let huge_file = QqStickerConfig {
            enabled: true,
            max_file_kb: 65_536,
            ..QqStickerConfig::default()
        };
        assert!(huge_file.validate().is_err());

        let enabled = QqStickerConfig {
            enabled: true,
            ..QqStickerConfig::default()
        };
        assert!(enabled.validate().is_ok());
    }

    #[test]
    fn byte_limit_follows_the_kilobyte_setting() {
        let config = QqStickerConfig {
            max_file_kb: 64,
            ..QqStickerConfig::default()
        };
        assert_eq!(config.max_file_bytes(), 65_536);
    }
}
