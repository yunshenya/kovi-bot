-- 一次性数据修正：清掉由 legacy 关系等级"投影"出来的关系张力。
--
-- 为什么需要：`[silence] enabled = true` 之后，`yunxi_relations.tension >= 0.6`
-- 会让静默门控在模型调用之前否决一条群消息——被 @ / 被引用也不例外。而
-- `plugins/model/src/yunxi/mod.rs` 早先把 legacy 的 `relationship_level` 反推成了
-- 张力（`tension = -affinity`）：等级 1（"礼貌、稍微正式"，同时是新用户默认值，
-- 线上八成档案都是它）→ 0.8，等级 2 → 0.6。于是每个新认识的人一建档就带着越线的
-- 张力，第一条 @ 她的话就被判"不接"（2026-09-14 02:09 群 687898502 实测）。
-- 代码侧已修（投影不再写张力），这个脚本处理已经落在库里的那一批。
--
-- 判据：`tension <= -affinity`，即"这一行的张力没有超过它自己的 affinity 所隐含的
-- 那个初值"。成立的理由是三条单调性：
--   * 投影写下的初值满足 `tension = -affinity`（同一个 level 算出来的两个数）；
--   * 此后 affinity 只会变大（`planner.rs` 的感恩两项）或向 0 漂移（365 天半衰期），
--     所以 `-affinity` 只减不增；
--   * tension 只被证据抬高，且漂移比 affinity 快得多（3 天半衰期）。
-- 于是"从来没有证据进过这一行"必然满足该式；攒过证据的行（tension 被抬到
-- `-affinity` 之上）一条都不碰。`+ 1e-6` 是给 f32 的 0.8 存进 DOUBLE PRECISION 后
-- 变成 0.800000011920929 留的余量。
--
-- 为什么不用 legacy 档案里**当前**的 level 反推初值：level 会随互动上升（感恩 +1），
-- 而 affinity/tension 停留在建档时的初值上。线上 qq 3096003763 就是这样——建档时
-- level 1（affinity -0.8），现在 level 2，按当前 level 算出的 0.6 会把它判成
-- "攒过证据"而漏掉。用行自己的 affinity 则不受影响。级别在台账里只作参考记录。
--
-- 幂等：修正过的 person_id 记进台账，重复执行是严格的空操作（否则台账里的人后来
-- 真的攒了证据、又恰好没超过 `-affinity`，会被二次清掉）。
--
-- 默认 dry-run：跑完 ROLLBACK，只打印会改哪些行。加 `-v apply=on` 才提交。

\set ON_ERROR_STOP on

BEGIN;

CREATE TABLE IF NOT EXISTS yunxi_relation_tension_seed_backfill (
    person_id      UUID PRIMARY KEY REFERENCES yunxi_persons(id) ON DELETE CASCADE,
    applied_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    tension_before DOUBLE PRECISION NOT NULL,
    affinity_at_fix DOUBLE PRECISION NOT NULL,
    legacy_level   SMALLINT
);

CREATE TEMP TABLE backfill_candidates ON COMMIT DROP AS
SELECT r.person_id,
       r.tension     AS tension_before,
       r.affinity    AS affinity_at_fix,
       r.updated_at  AS last_touched_at
FROM yunxi_relations r
WHERE r.affinity < 0
  AND r.tension > 0
  AND r.tension <= -r.affinity + 1e-6
  AND NOT EXISTS (
      SELECT 1 FROM yunxi_relation_tension_seed_backfill b WHERE b.person_id = r.person_id
  );

INSERT INTO yunxi_relation_tension_seed_backfill
    (person_id, tension_before, affinity_at_fix, legacy_level)
SELECT c.person_id,
       c.tension_before,
       c.affinity_at_fix,
       CASE
           WHEN (u.payload ->> 'relationship_level') ~ '^[0-9]+$'
           THEN (u.payload ->> 'relationship_level')::smallint
       END
FROM backfill_candidates c
LEFT JOIN yunxi_external_identities e
       ON e.person_id = c.person_id AND e.platform = 'qq'
LEFT JOIN kovi_bot_user_profiles u ON u.user_id::text = e.external_id;

-- `updated_at` 刻意不动：tension 归零后漂移是恒等变换，而那个时间戳还留着
-- "这一行最后一次被真实互动碰过是什么时候"的信息。
UPDATE yunxi_relations r
SET tension = 0
FROM backfill_candidates c
WHERE r.person_id = c.person_id;

\echo ''
\echo '=== 本次修正的行（dry-run 也会打印，但会回滚） ==='
SELECT e.external_id AS qq,
       u.payload ->> 'relationship_level'        AS legacy_level_now,
       round(c.affinity_at_fix::numeric, 4)      AS affinity,
       round(c.tension_before::numeric, 4)       AS tension_before,
       c.last_touched_at
FROM backfill_candidates c
LEFT JOIN yunxi_external_identities e
       ON e.person_id = c.person_id AND e.platform = 'qq'
LEFT JOIN kovi_bot_user_profiles u ON u.user_id::text = e.external_id
ORDER BY c.tension_before DESC;

\echo ''
\echo '=== 汇总 ==='
SELECT (SELECT count(*) FROM backfill_candidates)                  AS corrected_now,
       (SELECT count(*) FROM yunxi_relation_tension_seed_backfill) AS ledger_total,
       (SELECT count(*) FROM yunxi_relations WHERE tension >= 0.6) AS tension_ge_060_after;

\if :apply
\echo '>>> apply=on：提交'
COMMIT;
\else
\echo '>>> apply=off：回滚（dry-run，库里什么都没变）'
ROLLBACK;
\endif
