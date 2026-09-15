//! 卡死回合的处置出口：后台那一个「回收这个会话的回合」按钮。
//!
//! 为什么要有它：等待房间能**看出**卡住了（[`crate::model::waiting_room`]），影子档
//! 能**证明**卡在哪一步，但 2026-09-15 那次最终是靠重启进程解决的——那十个小时里
//! 没有任何一个按钮能让这个群先活过来。重启会清掉**所有**会话的排队；这个接口只动
//! 一个会话，而且只动"回合"不动"消息"（队列里的原文按顺序补答）。
//!
//! 三条边界：
//! - **只回收已经卡住的**：判据（静默超过阈值 + 回合等了够久 / 队列没人排空）与影子
//!   日志、自动回收共用一份。空闲或正常在跑的会话一律拒绝——否则它就成了一个
//!   "强行取消"的后门，误点一下就丢掉一轮正在生成的回复。
//! - **幂等**：代数必须还是观测到的那一代，连点两次的第二次会被拒（409）。
//! - **回执说清代价**：丢了几条已生成的回复、放了几条预留，都要报出来。

use super::{AdminState, ApiError};
use crate::model::ReplyScope;
use crate::model::waiting_room;
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// `POST /api/waiting-room/reclaim`
#[derive(Debug, Deserialize)]
pub(crate) struct ReclaimRequest {
    /// `group` / `private` / `scheduled` / `call`。
    kind: String,
    subject_id: i64,
    /// 前端观测到的那一代。缺省时后端自己再取一次当前代数——仍然要过"确实卡住"的判据。
    #[serde(default)]
    generation: Option<u64>,
}

pub(crate) async fn reclaim(
    State(_state): State<Arc<AdminState>>,
    Json(body): Json<ReclaimRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.subject_id <= 0 {
        return Err(ApiError::bad_request("会话编号必须是正整数"));
    }
    let scope = match body.kind.as_str() {
        "group" => ReplyScope::Group(body.subject_id),
        "private" => ReplyScope::Private(body.subject_id),
        "scheduled" => ReplyScope::Scheduled(body.subject_id),
        "call" => ReplyScope::Call(body.subject_id),
        other => {
            return Err(ApiError::bad_request(format!(
                "未知的会话类型 {other}（应为 group / private / scheduled / call）"
            )));
        }
    };
    let subject = waiting_room::describe(scope);

    let traffic = crate::config::get().traffic().clone();
    let stall_after = Duration::from_secs(traffic.window_stall_secs());
    let reclaim_after = Duration::from_secs(traffic.turn_reclaim_secs());
    let report = waiting_room::scope_report(scope, stall_after).await;

    let Some(reason) = waiting_room::reclaim_reason(&report, stall_after, reclaim_after) else {
        return Err(ApiError::bad_request(format!(
            "{subject} 现在不该回收（{}）。判据是「静默超过 {} 秒、且回合已经等了超过 {} 秒」，\
             空闲或正常在跑的会话不回收——这个按钮只处理已经卡住的回合。",
            report.summary,
            stall_after.as_secs(),
            reclaim_after.as_secs()
        )));
    };

    let generation = match body.generation {
        Some(generation) => generation,
        None => waiting_room::turn_generation(scope)
            .await
            .ok_or_else(|| ApiError::bad_request(format!("{subject} 没有在途回合，无从回收")))?,
    };

    let Some(reclaimed) = waiting_room::reclaim(scope, generation, reason).await else {
        // 幂等：观测与动手之间已经有人推进过代数（自动回收、另一次点击、用户自己撤回）。
        return Err(ApiError::conflict(
            "这一轮已经结束了（代数已推进），不需要回收；刷新一下页面即可",
        ));
    };

    Ok(Json(json!({
        "ok": true,
        "subject": subject,
        "reason": reclaimed.reason.label(),
        "generation": reclaimed.generation,
        "next_generation": reclaimed.next_generation,
        "discarded_prepared": reclaimed.discarded_prepared,
        "released_admissions": reclaimed.released_admissions,
        "detail": format!(
            "已回收 {subject} 的回合 {}（{}）：丢弃 {} 条已生成未发出的回复，释放 {} 条预留；\
             队列里的消息保持不动，会按顺序补答。",
            reclaimed.generation,
            reclaimed.reason.label(),
            reclaimed.discarded_prepared,
            reclaimed.released_admissions,
        ),
    })))
}
