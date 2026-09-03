//! Owner-facing World Model v4 commands (private chat): `#情境` /
//! `#world-status` — read-only observability (v4 §155, §244).
//!
//! Admin-only; shows ids/summary/counts, never message content, never
//! hidden chain-of-thought.

pub(crate) enum WorldCommand {
    Status,
    Help,
}

pub(crate) fn parse_world_command(text: &str) -> Option<WorldCommand> {
    let body = text.trim().trim_start_matches('#').trim();
    match body {
        "情境" | "world-status" | "世界状态" => Some(WorldCommand::Status),
        "情境帮助" | "world-help" => Some(WorldCommand::Help),
        _ => None,
    }
}

pub(crate) fn handle_world_command(text: &str) -> Option<String> {
    match parse_world_command(text)? {
        WorldCommand::Status => {
            let status = crate::yunxi::world_model::world_status_text();
            Some(status)
        }
        WorldCommand::Help => Some(
            "世界模型 v4 命令：\n#情境 或 #world-status —— 查看当前世界状态（活跃情境 / 场景 / 环境健康）"
                .to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_known_commands() {
        assert!(matches!(
            parse_world_command("#情境"),
            Some(WorldCommand::Status)
        ));
        assert!(matches!(
            parse_world_command("#world-status"),
            Some(WorldCommand::Status)
        ));
        assert!(matches!(
            parse_world_command("情境"),
            Some(WorldCommand::Status)
        ));
        assert!(parse_world_command("#情境 明天").is_none());
        assert!(parse_world_command("#记下 你好").is_none());
    }

    #[test]
    fn help_is_bounded_and_hints() {
        let help = handle_world_command("#情境帮助").expect("help");
        assert!(help.contains("情境"));
        assert!(help.chars().count() < 200);
    }

    #[test]
    fn status_without_runtime_is_graceful() {
        crate::yunxi::world_model::reset_for_tests();
        let status = handle_world_command("#情境").expect("status");
        // No storage configured (tests never enable the feature) → fallback.
        assert!(status.contains("World Model"));
    }
}
