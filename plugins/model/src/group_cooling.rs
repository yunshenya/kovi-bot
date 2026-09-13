//! 群级降温信号：一个群长期把她当外人时，降低她在这个群**未点名插话**的频率。
//!
//! 个人级门控（`yunxi::core_model` 的 `silence_gate_plan` + 关系张力）回答的是
//! "这个人一直不友好，这条不接"；这里回答的是另一个问题——"这个群还欢迎她
//! 主动开口吗"。两者的证据与后果都不同：
//!
//! - **证据是群级的**：几个人各说过一次"闭嘴"，没有谁的关系张力越过阈值，
//!   但整个群的气氛已经很清楚了；反过来，她插话后连续十几条消息没人搭理她，
//!   同样是"被当外人"，只是没有任何一条消息可供个人级门控记账。
//! - **后果是降频，不是静默**：命中只放弃**这一次**未点名抽样机会。被 `@`、
//!   被引用、被明确对话的回合完全不受影响（抽样路径根本不经过它们），也不存在
//!   "永远不说话"的状态——压力有上限、按半衰期自然回落，群里的正向互动还会
//!   主动把它拉回来。
//!
//! 判据分三层，与个人级门控同构，便于影子观察：
//! 1. [`group_cooling_evidence`] 把"她插话之后的观察结果"折算成一条证据（`Ignored`）；
//!    而"这条消息是不是在赶她"由 [`crate::relation_evidence`] 的模型判定给出——
//!    两层共用同一份判定，群级只负责把它翻译成压力。驱动观察的
//!    [`advance_ambient_watch`] 单独可测。
//! 2. [`apply_group_cooling_pressure`] / [`drift_group_cooling_pressure`] 决定
//!    这条证据让压力变成多少（有界、可衰减）。
//! 3. [`group_cooling_verdict`] 把压力翻译成"这一次抽样要不要跳过"。它不碰配置
//!    也不打日志：影子阶段由调用方拿开关决定是否真的跳过，判定照跑、行为不变。
//!
//! 已知误伤类别（写在这里，是为了让调参的人知道自己在调什么）：
//! 1. 两个人拌嘴时说"你别插嘴"——只要她刚插过话且还没人搭理她，就可能被判成
//!    对她的排挤。窗口很短、单次权重很低，需要累积才越线。
//! 2. 群里在挤兑另一个人（或另一个机器人）时，判定可能读成同一个意思。
//! 3. 一个纯粹聊嗨了的群、没人接她的话，会被记成"无人应答"。这是设计上接受的：
//!    她本来也不该在没人接话时越插越多。
//!
//! 三类都靠"阈值 + 半衰期 + 正向互动回暖"兜住，且后果只是少插几次话。

use crate::relation_evidence::RelationEvidence;
use std::time::{Duration, Instant};

/// 群级压力的上限。
///
/// 有上限才有"最坏情况"：压力再怎么涨，后果也只是"未点名抽样被跳过"，
/// 不会变成越来越长的静默，更不会封群。
pub(crate) const GROUP_COOLING_MAX_PRESSURE: f32 = 1.0;

/// 压力越过这条线，未点名抽样才被跳过。
///
/// 与个人级的 `SILENCE_TENSION_THRESHOLD` 取同一个刻度（0.6），这样两层的
/// 日志可以直接对比；但两者的量纲无关——个人级是关系张力，这里是证据计数。
pub(crate) const GROUP_COOLING_SKIP_THRESHOLD: f32 = 0.6;

/// 压力的半衰期：没有任何新证据时，一天回落一半。
///
/// 比关系张力的 3 天短得多，因为"这个群现在不欢迎她"是个比"这个人和我关系
/// 不好"更易变的状态；一天足以让一场集体起哄过去，又不至于让持续的排挤
/// 每隔几小时就重置。自然恢复正是靠它，而不是靠谁去解除。
pub(crate) const GROUP_COOLING_HALF_LIFE: Duration = Duration::from_secs(24 * 60 * 60);

/// 她插话后，群里至少又有这么多条消息没人搭理她，才算一次"无人应答"。
pub(crate) const IGNORED_MESSAGE_THRESHOLD: u32 = 12;

/// 并且至少要过这么久。一秒钟的连珠炮不该被读成"没人理她"：那更像大家
/// 正聊在兴头上，而不是她被晾着。
pub(crate) const IGNORED_MINIMUM_ELAPSED: Duration = Duration::from_secs(120);

/// 一条群消息（或一段观察）给"这个群欢不欢迎她"留下的证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupCoolingSignal {
    /// 当面对她的敌意：辱骂、驱赶、要求她停止发言。指向明确，权重最高。
    DirectedPushOut,
    /// 她刚插过话，群里有人（并没有对她说话）用"插嘴/没人问你"这类话否定
    /// 她这次开口。窗口最短，权重居中。
    AmbientPushOut,
    /// 她插话后持续无人应答。
    Ignored,
    /// 有人正常地跟她说话：群把她当成员，而不是外人。这是回暖通道的常态。
    Engaged,
    /// 对她释放善意（道谢/道歉），回暖最强。
    Warm,
}

impl GroupCoolingSignal {
    /// 把模型给的相处判定翻译成群级证据。
    ///
    /// 两层共用同一份判定（`crate::relation_evidence`）：指向她的敌意对个人级是
    /// 关系张力、对群级是"这个群在赶她"；中性则说明这个群还在正常跟她说话，
    /// 那是回暖通道的常态。
    pub(crate) const fn for_relation_evidence(evidence: RelationEvidence) -> Self {
        match evidence {
            RelationEvidence::Unfriendly => Self::DirectedPushOut,
            RelationEvidence::Warm => Self::Warm,
            RelationEvidence::Neutral => Self::Engaged,
        }
    }

    /// 基础权重：正 = 更冷，负 = 回暖。
    ///
    /// 数量级是量出来的，不是拍出来的：三个不同的人各说一次"闭嘴"（0.22×3
    /// ＝0.66）刚好越线，一个人的重复敌意因为打折（见 [`evidence_strength`]）
    /// 要很久才可能单独推过线——那本该由个人级门控处理。反向的证据更"便宜"：
    /// 群里的正常对话几次就能把压力拉回来，符合"有正向互动就回到正常频率"。
    const fn base_strength(self) -> f32 {
        match self {
            Self::DirectedPushOut => 0.22,
            Self::AmbientPushOut => 0.12,
            Self::Ignored => 0.08,
            Self::Engaged => -0.10,
            Self::Warm => -0.15,
        }
    }

    /// 日志用的稳定标签（便于 grep 与调参，不用 Debug 的驼峰）。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DirectedPushOut => "directed_push_out",
            Self::AmbientPushOut => "ambient_push_out",
            Self::Ignored => "ignored",
            Self::Engaged => "engaged",
            Self::Warm => "warm",
        }
    }

    /// 这条证据是"更冷"还是"回暖"。回暖证据在压力已经是 0 时是无操作。
    pub(crate) const fn is_push_out(self) -> bool {
        matches!(
            self,
            Self::DirectedPushOut | Self::AmbientPushOut | Self::Ignored
        )
    }
}

/// 一条证据在"谁给的"这个维度上的权重。
///
/// 同一个人反复**当面对她**的驱赶要打折：群级通道回答的是"这个群欢不欢迎她"，
/// 而"某个人一直不喜欢她"已经由个人级门控（关系张力）负责。这里只做近似去重
/// ——记住上一条驱赶证据来自谁，同一个人再来就按 45% 计——因为精确统计"有多少
/// 不同的人"要额外维护一个集合，而这条通道的后果本来就只是降频。
///
/// `AmbientPushOut` 不打折：它挂在"她刚插过话"这个群级时刻上，本来就不是
/// "某个人对她"的证据；`Ignored` 连发送者都没有。
pub(crate) fn evidence_strength(
    signal: GroupCoolingSignal,
    sender: Option<i64>,
    last_push_out_sender: Option<i64>,
) -> f32 {
    let repeated_sender = signal == GroupCoolingSignal::DirectedPushOut
        && sender.is_some()
        && sender == last_push_out_sender;
    if repeated_sender {
        signal.base_strength() * 0.45
    } else {
        signal.base_strength()
    }
}

/// 记一条证据之后的压力：有上限，也不为负。
pub(crate) fn apply_group_cooling_pressure(
    pressure: f32,
    signal: GroupCoolingSignal,
    sender: Option<i64>,
    last_push_out_sender: Option<i64>,
) -> f32 {
    sanitize_pressure(pressure + evidence_strength(signal, sender, last_push_out_sender))
}

/// 压力随时间的自然衰减（半衰期 [`GROUP_COOLING_HALF_LIFE`]）。
///
/// 与关系漂移一样只算不写：衰减是时间的函数，读的时候算一次就够，
/// 下一次写入会带上衰减后的值。
pub(crate) fn drift_group_cooling_pressure(pressure: f32, elapsed: Duration) -> f32 {
    let pressure = sanitize_pressure(pressure);
    if pressure == 0.0 || elapsed.is_zero() {
        return pressure;
    }
    let half_lives = elapsed.as_secs_f32() / GROUP_COOLING_HALF_LIFE.as_secs_f32();
    sanitize_pressure(pressure * 0.5_f32.powf(half_lives))
}

/// 群级降温的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupCoolingVerdict {
    /// 照常抽样。
    Allow,
    /// 这一次未点名抽样机会作废。
    Skip { reason: &'static str },
}

/// 群级降温判据本体：纯粹、可测，不碰配置也不打日志。
///
/// 与个人级 `silence_verdict` 同一个分层：返回 `Skip` 只代表代码这一侧同意
/// 跳过；影子观察阶段靠 `enabled` 做到"判定照跑、行为不变"。
pub(crate) fn group_cooling_verdict(pressure: f32, enabled: bool) -> GroupCoolingVerdict {
    if !enabled {
        return GroupCoolingVerdict::Allow;
    }
    if sanitize_pressure(pressure) >= GROUP_COOLING_SKIP_THRESHOLD {
        GroupCoolingVerdict::Skip {
            reason: "group_pressure",
        }
    } else {
        GroupCoolingVerdict::Allow
    }
}

/// 她插话之后的观察：还有多少条群消息没人搭理她。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AmbientInterjectionWatch {
    started_at: Instant,
    messages_since: u32,
}

impl AmbientInterjectionWatch {
    /// 她刚发出一次未点名插话时开始观察。
    pub(crate) const fn started_at(now: Instant) -> Self {
        Self {
            started_at: now,
            messages_since: 0,
        }
    }
}

/// 一条群消息对"她插话之后的观察"的影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AmbientWatchStep {
    /// 没有正在进行的观察：这条消息与它无关。
    Idle,
    /// 观察继续：群里又有人说话，但没搭理她。
    Waiting,
    /// 有人搭理她了：观察结束，这本身是回暖证据。
    Engaged,
    /// 条数与时长都过线：记一次"无人应答"，观察结束。
    Ignored,
}

/// 推进观察。`directed_to_her` 为真表示这条消息是对她说的。
///
/// 每插一次话只观察一轮：记满一次"无人应答"（或有人接话）就结束，
/// 不会用同一次插话反复记账。
pub(crate) fn advance_ambient_watch(
    watch: &mut Option<AmbientInterjectionWatch>,
    directed_to_her: bool,
    now: Instant,
) -> AmbientWatchStep {
    let Some(active) = watch.as_mut() else {
        return AmbientWatchStep::Idle;
    };
    if directed_to_her {
        *watch = None;
        return AmbientWatchStep::Engaged;
    }
    active.messages_since = active.messages_since.saturating_add(1);
    if active.messages_since >= IGNORED_MESSAGE_THRESHOLD
        && now.saturating_duration_since(active.started_at) >= IGNORED_MINIMUM_ELAPSED
    {
        *watch = None;
        return AmbientWatchStep::Ignored;
    }
    AmbientWatchStep::Waiting
}

/// 她插话之后的观察结果给出的群级证据。
///
/// 只处理**确定性**的那一维：她插过话、过了足够久、又有足够多条消息没人接她
/// （`Ignored`）。"这条消息是不是在赶她"是语言判断，由 `crate::relation_evidence`
/// 问模型一次，不在这里用词表猜——那张字面表维护不到底，线上实测一句"滚吧"就漏了。
///
/// `watch_step` 是这条消息推进观察后的结果——"她刚插过话、还没人搭理她"这个上下文
/// 只在这一刻存在；`Waiting` 分支需要的语言判断由调用方拿着这个窗口去发起。
pub(crate) fn group_cooling_evidence(watch_step: AmbientWatchStep) -> Option<GroupCoolingSignal> {
    match watch_step {
        AmbientWatchStep::Ignored => Some(GroupCoolingSignal::Ignored),
        _ => None,
    }
}

/// 把压力收进 `[0, 1]`；非有限值（数据库里的脏数据）按 0 处理。
fn sanitize_pressure(pressure: f32) -> f32 {
    if pressure.is_finite() {
        pressure.clamp(0.0, GROUP_COOLING_MAX_PRESSURE)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AmbientInterjectionWatch, AmbientWatchStep, GROUP_COOLING_HALF_LIFE,
        GROUP_COOLING_MAX_PRESSURE, GROUP_COOLING_SKIP_THRESHOLD, GroupCoolingSignal,
        GroupCoolingVerdict, IGNORED_MESSAGE_THRESHOLD, IGNORED_MINIMUM_ELAPSED,
        advance_ambient_watch, apply_group_cooling_pressure, drift_group_cooling_pressure,
        evidence_strength, group_cooling_evidence, group_cooling_verdict,
    };
    use crate::relation_evidence::RelationEvidence;
    use std::time::{Duration, Instant};

    #[test]
    fn pressure_is_bounded_in_both_directions() {
        let mut pressure = 0.0;
        for _ in 0..50 {
            pressure = apply_group_cooling_pressure(
                pressure,
                GroupCoolingSignal::DirectedPushOut,
                Some(1),
                None,
            );
        }
        assert_eq!(pressure, GROUP_COOLING_MAX_PRESSURE);

        let mut pressure = 0.4;
        for _ in 0..20 {
            pressure = apply_group_cooling_pressure(pressure, GroupCoolingSignal::Warm, None, None);
        }
        assert_eq!(pressure, 0.0);
    }

    #[test]
    fn pressure_decays_by_half_lives_and_never_gets_stuck() {
        let pressure =
            apply_group_cooling_pressure(0.9, GroupCoolingSignal::DirectedPushOut, Some(7), None);
        assert_eq!(
            drift_group_cooling_pressure(pressure, Duration::ZERO),
            pressure
        );
        assert!(
            (drift_group_cooling_pressure(pressure, GROUP_COOLING_HALF_LIFE) - pressure / 2.0)
                .abs()
                < 0.001
        );
        // 自然恢复：安静一天半以后压力已经低于阈值，不需要任何人解除。
        assert!(
            drift_group_cooling_pressure(pressure, GROUP_COOLING_HALF_LIFE * 2)
                < GROUP_COOLING_SKIP_THRESHOLD
        );
        assert!(drift_group_cooling_pressure(pressure, GROUP_COOLING_HALF_LIFE * 30) < 1e-4);
        // 时钟回拨/脏数据不该把压力推到界外。
        assert_eq!(
            drift_group_cooling_pressure(f32::NAN, GROUP_COOLING_HALF_LIFE),
            0.0
        );
    }

    #[test]
    fn repeated_hostility_from_one_sender_is_damped() {
        let first = evidence_strength(GroupCoolingSignal::DirectedPushOut, Some(9), None);
        let repeated = evidence_strength(GroupCoolingSignal::DirectedPushOut, Some(9), Some(9));
        let other_sender = evidence_strength(GroupCoolingSignal::DirectedPushOut, Some(8), Some(9));
        assert!(repeated < first);
        // 换一个人就恢复全价：群级通道关心的是"有多少人"，不是"骂了多少次"。
        assert_eq!(other_sender, first);
        // 回暖证据不打折，否则一个人的善意也会被连着忽略。
        assert_eq!(
            evidence_strength(GroupCoolingSignal::Warm, Some(9), Some(9)),
            evidence_strength(GroupCoolingSignal::Warm, Some(9), None)
        );
    }

    #[test]
    fn verdict_skips_only_when_enabled_and_over_threshold() {
        assert_eq!(
            group_cooling_verdict(GROUP_COOLING_MAX_PRESSURE, true),
            GroupCoolingVerdict::Skip {
                reason: "group_pressure"
            }
        );
        // 影子模式：判据成立，但结论是"照常抽样"——行为与现在完全一致。
        assert_eq!(
            group_cooling_verdict(GROUP_COOLING_MAX_PRESSURE, false),
            GroupCoolingVerdict::Allow
        );
        assert_eq!(
            group_cooling_verdict(GROUP_COOLING_SKIP_THRESHOLD - 0.01, true),
            GroupCoolingVerdict::Allow
        );
        assert_eq!(
            group_cooling_verdict(GROUP_COOLING_SKIP_THRESHOLD, true),
            GroupCoolingVerdict::Skip {
                reason: "group_pressure"
            }
        );
    }

    #[test]
    fn directed_messages_map_the_model_judgement_onto_group_evidence() {
        // 指向她的消息由模型判（`relation_evidence`），群级只负责翻译：敌意是
        // "这个群在赶她"，善意是最强回暖，中性说明群还在正常跟她说话。
        assert_eq!(
            GroupCoolingSignal::for_relation_evidence(RelationEvidence::Unfriendly),
            GroupCoolingSignal::DirectedPushOut
        );
        assert_eq!(
            GroupCoolingSignal::for_relation_evidence(RelationEvidence::Warm),
            GroupCoolingSignal::Warm
        );
        assert_eq!(
            GroupCoolingSignal::for_relation_evidence(RelationEvidence::Neutral),
            GroupCoolingSignal::Engaged
        );
    }

    #[test]
    fn ambient_dismissal_is_left_to_the_model_and_only_the_window_is_deterministic() {
        // 群级现在只做确定性那一维：她插过话、够久、够多条没人接 → Ignored。
        assert_eq!(
            group_cooling_evidence(AmbientWatchStep::Ignored),
            Some(GroupCoolingSignal::Ignored)
        );
        // "这句话是不是在否定她"是语言判断，交给模型，且必须带上"她刚插过话没人接"
        // 这个窗口；窗口之外同样的字面不记账——它完全可能是在说另一个人。
        // 这里钉住的是"窗口之外不问"：判定入口由 `model/group.rs` 的 waiting 分支守着。
        assert_eq!(group_cooling_evidence(AmbientWatchStep::Waiting), None);
        assert_eq!(group_cooling_evidence(AmbientWatchStep::Idle), None);
    }

    #[test]
    fn ignored_run_needs_enough_messages_and_enough_time() {
        let now = Instant::now();
        let mut watch = Some(AmbientInterjectionWatch::started_at(now));
        for _ in 0..IGNORED_MESSAGE_THRESHOLD - 1 {
            assert_eq!(
                advance_ambient_watch(&mut watch, false, now),
                AmbientWatchStep::Waiting
            );
        }
        assert!(watch.is_some());
        // 条数够了但时间没到：再等等，不算"被晾着"。
        assert_eq!(
            advance_ambient_watch(&mut watch, false, now + Duration::from_secs(5)),
            AmbientWatchStep::Waiting
        );
        assert!(watch.is_some());
        assert_eq!(
            advance_ambient_watch(&mut watch, false, now + IGNORED_MINIMUM_ELAPSED),
            AmbientWatchStep::Ignored
        );
        // 一轮观察只记一次，不会用同一次插话反复记账。
        assert_eq!(
            advance_ambient_watch(&mut watch, false, now + IGNORED_MINIMUM_ELAPSED),
            AmbientWatchStep::Idle
        );
    }

    #[test]
    fn someone_talking_to_her_ends_the_watch_immediately() {
        let now = Instant::now();
        let mut watch = Some(AmbientInterjectionWatch::started_at(now));
        assert_eq!(
            advance_ambient_watch(&mut watch, false, now),
            AmbientWatchStep::Waiting
        );
        assert_eq!(
            advance_ambient_watch(&mut watch, true, now),
            AmbientWatchStep::Engaged
        );
        assert_eq!(watch, None);
        assert_eq!(
            advance_ambient_watch(&mut watch, false, now),
            AmbientWatchStep::Idle
        );
    }

    #[test]
    fn a_bounded_number_of_hostile_groups_can_trip_the_threshold() {
        // 三个不同的人各一次敌意：越线（这是群级通道真正要抓的形状）。
        let mut pressure = 0.0;
        let mut last = None;
        for sender in [1, 2, 3] {
            pressure = apply_group_cooling_pressure(
                pressure,
                GroupCoolingSignal::DirectedPushOut,
                Some(sender),
                last,
            );
            last = Some(sender);
        }
        assert!(pressure >= GROUP_COOLING_SKIP_THRESHOLD);
        assert_eq!(
            group_cooling_verdict(pressure, true),
            GroupCoolingVerdict::Skip {
                reason: "group_pressure"
            }
        );
        // 同一个人的三条敌意推不过线，也不会推不过线太久。
        let mut pressure = 0.0;
        for _ in 0..3 {
            pressure = apply_group_cooling_pressure(
                pressure,
                GroupCoolingSignal::DirectedPushOut,
                Some(1),
                Some(1),
            );
        }
        assert!(pressure < GROUP_COOLING_SKIP_THRESHOLD);
    }
}
