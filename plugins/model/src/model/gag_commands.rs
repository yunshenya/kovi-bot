//! Owner-facing gag-ledger commands (private chat): `#记下` / `#账本` /
//! `#还账` / `#清账本`, plus the bounded reply-context injection so she
//! "remembers her debts" while chatting.

use crate::yunxi;
use crate::yunxi::gag_store::{GagEntry, GagKind, GagScope};
use uuid::Uuid;

pub(crate) enum GagCommand {
    Record(String),
    List,
    Fulfill(String),
    Clear,
    Help,
}

pub(crate) fn parse_gag_command(text: &str) -> Option<GagCommand> {
    let body = text.trim().trim_start_matches('#').trim();
    if let Some(rest) = body.strip_prefix("记下") {
        let rest = rest.trim();
        return (!rest.is_empty()).then(|| GagCommand::Record(rest.to_string()));
    }
    if let Some(rest) = body.strip_prefix("还账") {
        let rest = rest.trim();
        return (!rest.is_empty()).then(|| GagCommand::Fulfill(rest.to_string()));
    }
    match body {
        "账本" | "查账" => Some(GagCommand::List),
        "清账本" | "清账" | "删账" => Some(GagCommand::Clear),
        "账本帮助" => Some(GagCommand::Help),
        _ => None,
    }
}

pub(crate) async fn handle_gag_command(person_key: String, text: &str) -> Option<String> {
    let command = parse_gag_command(text)?;
    let store = yunxi::gag_store()?;
    let scope = GagScope::Person(person_key);
    match command {
        GagCommand::Record(raw) => {
            let kind = infer_kind(&raw);
            let id = store.add(scope, kind, raw.trim(), 60).await.ok()?;
            Some(format!(
                "记下啦（{}）：{}。等兑现了跟我说“还账 {}”就行～",
                kind_label(kind),
                raw.trim(),
                short_id(id)
            ))
        }
        GagCommand::List => {
            let entries = store.list_open(scope, 20).await.ok()?;
            if entries.is_empty() {
                Some("账本是空的，暂时没有还没兑现的承诺或梗～".to_string())
            } else {
                let lines = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        format!(
                            "{}. [{}] {}（{}）",
                            index + 1,
                            kind_label(entry.kind),
                            entry.text,
                            short_id(entry.id)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(format!("你的账本（{} 条）：\n{}", entries.len(), lines))
            }
        }
        GagCommand::Fulfill(id_text) => {
            let id_text = id_text.trim();
            if let Ok(id) = Uuid::parse_str(id_text)
                && store.fulfill(id).await.ok()?
            {
                return Some("还账成功 ✅ 这条清啦～".to_string());
            }
            if let Some(id) = store.fulfill_by_prefix(id_text).await.ok()? {
                Some(format!("还账成功 ✅（{}）这条清啦～", short_id(id)))
            } else {
                Some(
                    "没找到这条账：用 #账本 里列出的短 id（如 3f2a1c9b）或完整 id 再来一次。"
                        .to_string(),
                )
            }
        }
        GagCommand::Clear => {
            let removed = store.delete_for_scope(scope).await.ok()?;
            Some(format!("已清空你的账本（{} 条，含已还的）～", removed))
        }
        GagCommand::Help => Some(
            "账本用法：\n#记下 <承诺/梗/记仇>\n#账本\n#还账 <短id>\n#清账本\n#账本帮助".to_string(),
        ),
    }
}

/// Bounded "gag ledger" context block for reply injection.
pub(crate) async fn ledger_context_for(person_key: &str) -> Option<String> {
    let store = yunxi::gag_store()?;
    let entries = store
        .list_open(GagScope::Person(person_key.to_string()), 5)
        .await
        .ok()?;
    if entries.is_empty() {
        return None;
    }
    let lines = entries.iter().map(gag_line).collect::<Vec<_>>().join("\n");
    Some(format!(
        "<角色账本 data-only=\"true\">\n你还欠着/记着这些：\n{lines}\n</角色账本>"
    ))
}

fn gag_line(entry: &GagEntry) -> String {
    format!("- [{}] {}", kind_label(entry.kind), entry.text)
}

fn infer_kind(raw: &str) -> GagKind {
    if raw.contains('欠') || raw.contains("答应") || raw.contains("承诺") {
        GagKind::Promise
    } else if raw.contains("记仇") || raw.contains("讨厌") || raw.contains("等着瞧") {
        GagKind::Grudge
    } else {
        GagKind::Gag
    }
}

fn kind_label(kind: GagKind) -> &'static str {
    match kind {
        GagKind::Promise => "承诺",
        GagKind::Gag => "梗",
        GagKind::Grudge => "记仇",
    }
}

fn short_id(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::parse_gag_command;

    #[test]
    fn parses_owner_ledger_commands() {
        assert!(matches!(
            parse_gag_command("#记下 芸汐欠我100个冷笑话"),
            Some(super::GagCommand::Record(text)) if text.contains("冷笑话")
        ));
        assert!(matches!(
            parse_gag_command("账本"),
            Some(super::GagCommand::List)
        ));
        assert!(matches!(
            parse_gag_command("#还账 3f2a1c9b"),
            Some(super::GagCommand::Fulfill(id)) if id == "3f2a1c9b"
        ));
        assert!(matches!(
            parse_gag_command("#清账本"),
            Some(super::GagCommand::Clear)
        ));
        assert!(parse_gag_command("今天天气不错").is_none());
        assert!(parse_gag_command("#记下   ").is_none());
    }
}
