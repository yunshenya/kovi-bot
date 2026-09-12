//! 多路检索结果的融合。
//!
//! 记忆检索本来只有一路（词面命中 + 重要性 + 时间）。加语义那一路之后就有**两路**，
//! 而"两路怎么合"不是细节，是一个会静默出错的接缝——Hindsight 为此专门写了第二套
//! 融合策略，并在 docstring 里记下了他们踩的坑（原文大意）：
//!
//! > RRF 按**各路倒数名次之和**打分，于是"在某一路排第 1、在其它路缺席或靠后"的结果
//! > 会被平均下去。这正是 consolidation 去重的失效模式：那个该被合并的"孪生"观测
//! > 在语义那一路排第 1，却与来源事实没有图谱链接、词面重叠也少，RRF 把它压到
//! > 召回预算之下，模型根本看不到它，于是**造了个重复的**。
//!
//! 所以这里提供两种策略：
//!
//! - [`FusionStrategy::Rrf`]：经典倒数名次融合，稳健、偏爱"多路都认可"的结果；
//! - [`FusionStrategy::Interleave`]：轮转取每路的第 1、第 2……，**保证每一路的头部
//!   都拿到名额**。语义那一路排第 1 的东西不会被别的路挤掉。
//!
//! 默认用 interleave：在我们这个场景里，"某一路强烈认为相关"比"多路温和同意"更值得
//! 保住——因为被挤掉的代价是**静默的**（她记不起来，而没有任何日志会说）。

/// 融合策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FusionStrategy {
    /// 倒数名次融合：`score(d) = Σ 1/(k + rank(d))`。
    ///
    /// 生产默认用 [`FusionStrategy::Interleave`]；RRF 保留下来是因为它是这个问题的
    /// 经典解，而且**测试要靠它复现那条失效模式**（见 `interleave_keeps_...`）——
    /// 没有它就没法证明 interleave 到底防住了什么。
    #[allow(dead_code)]
    Rrf,
    /// 轮转融合：每路的第 1 名、再每路的第 2 名……去重后依次放入。
    Interleave,
}

/// RRF 公式里的常数。60 是文献与 Hindsight 都在用的默认值。
const RRF_K: f32 = 60.0;

/// 把多路结果融成一路，最多 `limit` 条。
///
/// `arms` 里每一路都应当已经按各自的相关性从好到差排好序。返回的 id 不重复。
pub(crate) fn fuse(arms: &[Vec<String>], strategy: FusionStrategy, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    match strategy {
        FusionStrategy::Rrf => fuse_rrf(arms, limit),
        FusionStrategy::Interleave => fuse_interleave(arms, limit),
    }
}

fn fuse_rrf(arms: &[Vec<String>], limit: usize) -> Vec<String> {
    let mut scores: Vec<(String, f32)> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for arm in arms {
        for (index, id) in arm.iter().enumerate() {
            let rank = (index + 1) as f32;
            let contribution = 1.0 / (RRF_K + rank);
            match seen.get(id) {
                Some(position) => scores[*position].1 += contribution,
                None => {
                    seen.insert(id.clone(), scores.len());
                    scores.push((id.clone(), contribution));
                }
            }
        }
    }
    // 分数相同时按 id 排序，保证结果确定（测试与线上都不会因为哈希顺序抖动）。
    scores.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scores.into_iter().take(limit).map(|(id, _)| id).collect()
}

fn fuse_interleave(arms: &[Vec<String>], limit: usize) -> Vec<String> {
    let longest = arms.iter().map(Vec::len).max().unwrap_or(0);
    let mut ordered: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for rank in 0..longest {
        // 每一路的第 rank 名依次拿一个名额：路的顺序就是优先级（语义优先）。
        for arm in arms {
            let Some(id) = arm.get(rank) else {
                continue;
            };
            if seen.insert(id.as_str()) {
                ordered.push(id.clone());
                if ordered.len() >= limit {
                    return ordered;
                }
            }
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn rrf_favours_what_every_arm_agrees_on() {
        // A 在两条路都靠前；B 只在一条路第一。RRF 会把 A 排在前面——这是它的优点。
        let fused = fuse(
            &[arm(&["b", "x", "a"]), arm(&["x", "a", "b"])],
            FusionStrategy::Rrf,
            3,
        );
        assert_eq!(fused[0], "x", "两路都第 2 的 x 最稳");
        assert_eq!(fused.len(), 3);
        // 不重复。
        let mut sorted = fused.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), fused.len());
    }

    /// 这条测试就是 interleave 存在的理由，也是 Hindsight docstring 里那个坑的复现。
    #[test]
    fn interleave_keeps_an_arm_leader_that_rrf_would_bury() {
        // 语义那一路的头部是 "twin"（该被看到的那条），但它在其它两路完全缺席；
        // 其它路的候选彼此重合、分数堆得更高。
        let semantic = arm(&["twin", "s2", "s3"]);
        let lexical = arm(&["l1", "l2", "l3", "l4", "l5", "l6", "s2", "s3"]);
        let temporal = arm(&["l1", "l2", "l3", "l4", "l5", "l6", "s2"]);
        let arms = [semantic, lexical, temporal];

        // 只留 3 条时，RRF 会把 twin 挤出去——它只在一条路出现。
        let rrf = fuse(&arms, FusionStrategy::Rrf, 3);
        assert!(
            !rrf.contains(&"twin".to_string()),
            "RRF 应当把只在单路出现的头部挤掉（这正是要防的失效模式），实际 {rrf:?}"
        );

        // interleave 则保证每一路的头部都拿到名额。
        let interleaved = fuse(&arms, FusionStrategy::Interleave, 3);
        assert_eq!(
            interleaved[0], "twin",
            "每一路的第一名必须都拿到名额，语义那一路优先"
        );
        assert!(interleaved.contains(&"l1".to_string()));
    }

    #[test]
    fn fusion_is_bounded_deduplicated_and_handles_empty_arms() {
        let arms = [arm(&["a", "b"]), Vec::new(), arm(&["b", "c"])];
        for strategy in [FusionStrategy::Rrf, FusionStrategy::Interleave] {
            let fused = fuse(&arms, strategy, 10);
            assert_eq!(fused.len(), 3, "{strategy:?} 应当去重");
            // 空的那一路不该让融合崩溃或漏掉其它路。
            assert!(fused.contains(&"a".to_string()));
            assert!(fused.contains(&"c".to_string()));
            // limit 生效。
            assert_eq!(fuse(&arms, strategy, 1).len(), 1);
            assert!(fuse(&arms, strategy, 0).is_empty());
        }
        // 全空也不能炸。
        assert!(fuse(&[], FusionStrategy::Interleave, 5).is_empty());
        assert!(fuse(&[Vec::new()], FusionStrategy::Rrf, 5).is_empty());
    }

    #[test]
    fn interleave_with_one_arm_is_just_that_arm() {
        // 只有词面一路时（嵌入服务没起来），融合必须退化成原样，不能改变现有行为。
        let single = arm(&["a", "b", "c", "d"]);
        assert_eq!(
            fuse(std::slice::from_ref(&single), FusionStrategy::Interleave, 3),
            arm(&["a", "b", "c"])
        );
        assert_eq!(
            fuse(std::slice::from_ref(&single), FusionStrategy::Rrf, 4).len(),
            4,
            "单路时 RRF 也不该丢东西"
        );
    }
}
