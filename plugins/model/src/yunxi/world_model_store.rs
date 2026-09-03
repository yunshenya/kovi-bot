//! Persistent World Model v4 store (host-side infrastructure).
//!
//! SQL is infrastructure (v4 §126): the core stays platform-neutral; this
//! module maps the in-memory [`yunxi_core::WorldModel`] onto Postgres tables
//! for restart recovery (v4 §130) and gives data deletion a real boundary
//! (v4 §241–§242). Schema changes are additive + idempotent and run under
//! the same advisory-lock migration gate as every other Yunxi table.

use sqlx_core::query::query;
use sqlx_core::row::Row;
use sqlx_core::transaction::Transaction;
use sqlx_postgres::{PgPool, Postgres};
use yunxi_core::WorldModel;
use yunxi_core::world_model::{
    EntityKind, EntityState, Hypothesis, HypothesisStatus, Observation, ObservationKind,
    ObservationSource, Situation, SituationKind, SituationState, SocialSceneKind, SocialSceneState,
    StateProperty, UncertaintyType, WorldScope, WorldUncertainty,
};
use yunxi_core::{ConversationId, PersonId};

// scope encoding for `scope_kind` columns.
const SCOPE_PERSON: &str = "person";
const SCOPE_CONVERSATION: &str = "conversation";
const SCOPE_GLOBAL: &str = "global";

pub(crate) struct PostgresWorldModelStore {
    pool: PgPool,
}

impl PostgresWorldModelStore {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn initialize_schema(&self) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_world_observations (
                id UUID PRIMARY KEY,
                source_event_id UUID NOT NULL,
                scope_kind TEXT NOT NULL CHECK (scope_kind IN ('person', 'conversation', 'global')),
                scope_id UUID,
                kind TEXT NOT NULL,
                source TEXT NOT NULL,
                content TEXT NOT NULL,
                facet TEXT,
                confidence REAL NOT NULL,
                observed_at TIMESTAMPTZ NOT NULL,
                expires_at TIMESTAMPTZ,
                version BIGINT NOT NULL,
                CHECK ((scope_kind = 'global' AND scope_id IS NULL) OR (scope_kind <> 'global' AND scope_id IS NOT NULL))
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_world_observations_scope_idx
             ON yunxi_world_observations (scope_kind, scope_id, observed_at DESC)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_world_observations_expiry_idx
             ON yunxi_world_observations (expires_at)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_world_entities (
                id UUID PRIMARY KEY,
                kind TEXT NOT NULL,
                linked_person UUID,
                linked_conversation UUID,
                confidence REAL NOT NULL,
                last_observed_at TIMESTAMPTZ NOT NULL,
                version BIGINT NOT NULL,
                properties JSONB NOT NULL DEFAULT '[]'::jsonb
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_world_entities_scope_idx
             ON yunxi_world_entities (linked_person, linked_conversation)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_world_situations (
                id UUID PRIMARY KEY,
                kind TEXT NOT NULL,
                state TEXT NOT NULL,
                detail TEXT,
                conversation_id UUID,
                persons JSONB NOT NULL DEFAULT '[]'::jsonb,
                participants JSONB NOT NULL DEFAULT '[]'::jsonb,
                related_goals JSONB NOT NULL DEFAULT '[]'::jsonb,
                related_open_loops JSONB NOT NULL DEFAULT '[]'::jsonb,
                confidence REAL NOT NULL,
                started_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                ended_at TIMESTAMPTZ,
                version BIGINT NOT NULL
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_world_situations_conv_idx
             ON yunxi_world_situations (conversation_id, updated_at DESC)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_world_hypotheses (
                id UUID PRIMARY KEY,
                proposition_key TEXT NOT NULL,
                proposition_text TEXT NOT NULL,
                scope_kind TEXT NOT NULL CHECK (scope_kind IN ('person', 'conversation', 'global')),
                scope_id UUID,
                confidence REAL NOT NULL,
                status TEXT NOT NULL,
                evidence_for JSONB NOT NULL DEFAULT '[]'::jsonb,
                evidence_against JSONB NOT NULL DEFAULT '[]'::jsonb,
                created_at TIMESTAMPTZ NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL,
                expires_at TIMESTAMPTZ,
                version BIGINT NOT NULL
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_world_hypotheses_scope_idx
             ON yunxi_world_hypotheses (scope_kind, scope_id, status)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_world_scenes (
                conversation_id UUID PRIMARY KEY,
                scene_kind TEXT NOT NULL,
                activity_level REAL NOT NULL,
                interruption_cost REAL NOT NULL,
                bot_addressed BOOLEAN NOT NULL,
                participants JSONB NOT NULL DEFAULT '[]'::jsonb,
                floor JSONB NOT NULL DEFAULT '[]'::jsonb,
                recent_speakers JSONB NOT NULL DEFAULT '[]'::jsonb,
                conversation_version BIGINT NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_world_uncertainties (
                id UUID PRIMARY KEY,
                uncertainty_type TEXT NOT NULL,
                scope_kind TEXT NOT NULL CHECK (scope_kind IN ('person', 'conversation', 'global')),
                scope_id UUID,
                note TEXT NOT NULL,
                observed_at TIMESTAMPTZ NOT NULL,
                expires_at TIMESTAMPTZ,
                version BIGINT NOT NULL
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            r#"CREATE TABLE IF NOT EXISTS yunxi_world_causal (
                id UUID PRIMARY KEY,
                cause_kind TEXT NOT NULL,
                cause_label TEXT NOT NULL,
                effect_kind TEXT NOT NULL,
                effect_label TEXT NOT NULL,
                strength REAL NOT NULL,
                confidence REAL NOT NULL,
                source TEXT NOT NULL,
                scope_kind TEXT NOT NULL CHECK (scope_kind IN ('global', 'tool_specific', 'person_specific', 'conversation_specific', 'host_specific')),
                scope_id TEXT,
                evidence_occurrences BIGINT NOT NULL,
                version BIGINT NOT NULL
            )"#,
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE INDEX IF NOT EXISTS yunxi_world_causal_scope_idx
             ON yunxi_world_causal (scope_kind, scope_id)",
        )
        .execute(&mut *transaction)
        .await?;
        query(
            "CREATE TABLE IF NOT EXISTS yunxi_world_meta (
                key TEXT PRIMARY KEY,
                value JSONB NOT NULL
            )",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Persist the whole bounded world state (snapshot upsert, v4 §128:
    /// long-lived state in tables, transient state stays runtime-only).
    pub(crate) async fn save_world(&self, world: &WorldModel) -> anyhow::Result<()> {
        world
            .validate()
            .map_err(|error| anyhow::anyhow!("World Model 校验失败，拒绝持久化: {error}"))?;
        let mut transaction = self.pool.begin().await?;
        super::schema::lock(&mut transaction).await?;
        for table in [
            "yunxi_world_observations",
            "yunxi_world_entities",
            "yunxi_world_situations",
            "yunxi_world_hypotheses",
            "yunxi_world_scenes",
            "yunxi_world_uncertainties",
            "yunxi_world_causal",
        ] {
            query(&format!("DELETE FROM {table}"))
                .execute(&mut *transaction)
                .await?;
        }
        for observation in world.observations() {
            let (scope_kind, scope_id) = encode_scope(observation.scope());
            query(
                r#"INSERT INTO yunxi_world_observations
                   (id, source_event_id, scope_kind, scope_id, kind, source, content,
                    facet, confidence, observed_at, expires_at, version)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
            )
            .bind(observation.id().into_uuid())
            .bind(observation.source_event_id().into_uuid())
            .bind(scope_kind)
            .bind(scope_id)
            .bind(enum_label(observation.kind()))
            .bind(enum_label(observation.source()))
            .bind(observation.payload().content())
            .bind(observation.payload().facet())
            .bind(observation.confidence())
            .bind(observation.observed_at())
            .bind(observation.expires_at())
            .bind(observation.version() as i64)
            .execute(&mut *transaction)
            .await?;
        }
        for entity in world.entities().iter() {
            query(
                r#"INSERT INTO yunxi_world_entities
                   (id, kind, linked_person, linked_conversation, confidence,
                    last_observed_at, version, properties)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
            )
            .bind(entity.id().into_uuid())
            .bind(entity_kind_label(entity.kind()))
            .bind(entity.linked_person().map(PersonId::into_uuid))
            .bind(entity.linked_conversation().map(ConversationId::into_uuid))
            .bind(entity.confidence())
            .bind(entity.last_observed_at())
            .bind(entity.version() as i64)
            .bind(serde_json::to_value(entity.properties())?)
            .execute(&mut *transaction)
            .await?;
        }
        for situation in world.situations() {
            query(
                r#"INSERT INTO yunxi_world_situations
                   (id, kind, state, detail, conversation_id, persons, participants,
                    related_goals, related_open_loops, confidence, started_at,
                    updated_at, ended_at, version)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"#,
            )
            .bind(situation.id().into_uuid())
            .bind(situation_kind_label(situation.kind()))
            .bind(situation_state_label(situation.state()))
            .bind(situation.detail())
            .bind(situation.conversation_id().map(ConversationId::into_uuid))
            .bind(serde_json::to_value(situation.persons())?)
            .bind(serde_json::to_value(situation.participants())?)
            .bind(serde_json::to_value(situation.related_goals())?)
            .bind(serde_json::to_value(situation.related_open_loops())?)
            .bind(situation.confidence())
            .bind(situation.started_at())
            .bind(situation.updated_at())
            .bind(situation.ended_at())
            .bind(situation.version() as i64)
            .execute(&mut *transaction)
            .await?;
        }
        for hypothesis in world.hypotheses() {
            let (scope_kind, scope_id) = encode_scope(hypothesis.scope());
            query(
                r#"INSERT INTO yunxi_world_hypotheses
                   (id, proposition_key, proposition_text, scope_kind, scope_id,
                    confidence, status, evidence_for, evidence_against, created_at,
                    updated_at, expires_at, version)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"#,
            )
            .bind(hypothesis.id().into_uuid())
            .bind(hypothesis.proposition().key())
            .bind(hypothesis.proposition().text())
            .bind(scope_kind)
            .bind(scope_id)
            .bind(hypothesis.confidence())
            .bind(hypothesis_status_label(hypothesis.status()))
            .bind(serde_json::to_value(hypothesis.evidence_for())?)
            .bind(serde_json::to_value(hypothesis.evidence_against())?)
            .bind(hypothesis.created_at())
            .bind(hypothesis.updated_at())
            .bind(hypothesis.expires_at())
            .bind(hypothesis.version() as i64)
            .execute(&mut *transaction)
            .await?;
        }
        for scene in world.social_scenes() {
            query(
                r#"INSERT INTO yunxi_world_scenes
                   (conversation_id, scene_kind, activity_level, interruption_cost,
                    bot_addressed, participants, floor, recent_speakers,
                    conversation_version, updated_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
            )
            .bind(scene.conversation_id().into_uuid())
            .bind(scene_kind_label(scene.scene_kind()))
            .bind(scene.activity_level())
            .bind(scene.interruption_cost())
            .bind(scene.bot_addressed())
            .bind(serde_json::to_value(scene.active_participants())?)
            .bind(serde_json::to_value(scene.current_floor())?)
            .bind(serde_json::to_value(scene.recent_speaking_order())?)
            .bind(scene.conversation_version() as i64)
            .bind(scene.updated_at())
            .execute(&mut *transaction)
            .await?;
        }
        for uncertainty in world.uncertainties() {
            let (scope_kind, scope_id) = encode_scope(uncertainty.scope());
            query(
                r#"INSERT INTO yunxi_world_uncertainties
                   (id, uncertainty_type, scope_kind, scope_id, note, observed_at,
                    expires_at, version)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
            )
            .bind(uncertainty.id().into_uuid())
            .bind(uncertainty_type_label(uncertainty.uncertainty_type()))
            .bind(scope_kind)
            .bind(scope_id)
            .bind(uncertainty.note())
            .bind(uncertainty.observed_at())
            .bind(uncertainty.expires_at())
            .bind(uncertainty.version() as i64)
            .execute(&mut *transaction)
            .await?;
        }
        query(
            r#"INSERT INTO yunxi_world_meta (key, value) VALUES ('version', $1)
               ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"#,
        )
        .bind(serde_json::json!({ "version": world.version() }))
        .execute(&mut *transaction)
        .await?;
        for relation in world.causal().relations() {
            let (scope_kind, scope_id) = encode_causal_scope(relation.scope());
            query(
                r#"INSERT INTO yunxi_world_causal
                   (id, cause_kind, cause_label, effect_kind, effect_label, strength,
                    confidence, source, scope_kind, scope_id, evidence_occurrences, version)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
            )
            .bind(relation.id().into_uuid())
            .bind(pattern_kind_label(relation.cause().kind()))
            .bind(relation.cause().label())
            .bind(pattern_kind_label(relation.effect().kind()))
            .bind(relation.effect().label())
            .bind(relation.strength())
            .bind(relation.confidence())
            .bind(causal_source_label(relation.source()))
            .bind(scope_kind)
            .bind(scope_id)
            .bind(relation.evidence_occurrences() as i64)
            .bind(relation.version() as i64)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Restore the persisted world (v4 §130). `None` when no state exists.
    pub(crate) async fn load_world(&self) -> anyhow::Result<Option<WorldModel>> {
        let mut observations = Vec::new();
        let rows = query("SELECT * FROM yunxi_world_observations")
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let scope = decode_scope(
                row.try_get::<String, _>("scope_kind")?.as_str(),
                row.try_get::<Option<uuid::Uuid>, _>("scope_id")?,
            )?;
            let observation = Observation::new(
                yunxi_core::ObservationId::from_uuid(row.try_get("id")?),
                yunxi_core::EventId::from_uuid(row.try_get("source_event_id")?),
                scope,
                decode_observation_kind(row.try_get::<String, _>("kind")?),
                decode_observation_source(row.try_get::<String, _>("source")?),
                yunxi_core::world_model::ObservationPayload::new(
                    row.try_get::<String, _>("content")?,
                    row.try_get::<Option<String>, _>("facet")?
                        .map(|f| f as String),
                )?,
                row.try_get::<f32, _>("confidence")?,
                row.try_get("observed_at")?,
                row.try_get("expires_at")?,
            )?;
            observations.push(observation);
        }
        let mut entities = Vec::new();
        let rows = query("SELECT * FROM yunxi_world_entities")
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let properties: Vec<StateProperty> = serde_json::from_value(row.try_get("properties")?)
                .map_err(json_error("yunxi_world_entities.properties"))?;
            let mut entity = EntityState::new(
                yunxi_core::EntityId::from_uuid(row.try_get("id")?),
                decode_entity_kind(row.try_get::<String, _>("kind")?),
                row.try_get::<Option<uuid::Uuid>, _>("linked_person")?
                    .map(PersonId::from_uuid),
                row.try_get::<Option<uuid::Uuid>, _>("linked_conversation")?
                    .map(ConversationId::from_uuid),
                row.try_get::<f32, _>("confidence")?,
                row.try_get("last_observed_at")?,
            )?;
            for property in properties {
                entity.apply(
                    &yunxi_core::world_model::EntityUpdateAction::Set(property),
                    entity.last_observed_at(),
                )?;
            }
            entities.push(entity);
        }
        let mut situations = Vec::new();
        let rows = query("SELECT * FROM yunxi_world_situations")
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let situation = Situation::restore(
                yunxi_core::SituationId::from_uuid(row.try_get("id")?),
                decode_situation_kind(row.try_get::<String, _>("kind")?),
                decode_situation_state(row.try_get::<String, _>("state")?),
                row.try_get::<Option<String>, _>("detail")?,
                serde_json::from_value(row.try_get("participants")?)
                    .map_err(json_error("situation participants"))?,
                serde_json::from_value(row.try_get("persons")?)
                    .map_err(json_error("situation persons"))?,
                row.try_get::<Option<uuid::Uuid>, _>("conversation_id")?
                    .map(ConversationId::from_uuid),
                serde_json::from_value(row.try_get("related_goals")?)
                    .map_err(json_error("situation related_goals"))?,
                serde_json::from_value(row.try_get("related_open_loops")?)
                    .map_err(json_error("situation related_open_loops"))?,
                row.try_get::<f32, _>("confidence")?,
                row.try_get("started_at")?,
                row.try_get("updated_at")?,
                row.try_get("ended_at")?,
                row.try_get::<i64, _>("version")? as u64,
            )?;
            situations.push(situation);
        }
        let mut hypotheses = Vec::new();
        let rows = query("SELECT * FROM yunxi_world_hypotheses")
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let proposition = yunxi_core::world_model::WorldProposition::new(
                row.try_get::<String, _>("proposition_text")?,
            )?;
            let mut hypothesis = Hypothesis::new(
                yunxi_core::HypothesisId::from_uuid(row.try_get("id")?),
                proposition,
                decode_scope(
                    row.try_get::<String, _>("scope_kind")?.as_str(),
                    row.try_get::<Option<uuid::Uuid>, _>("scope_id")?,
                )?,
                row.try_get::<f32, _>("confidence")?,
                row.try_get("created_at")?,
                row.try_get("expires_at")?,
            )?;
            let evidence_for: Vec<yunxi_core::ObservationId> =
                serde_json::from_value(row.try_get("evidence_for")?)
                    .map_err(json_error("evidence_for"))?;
            let evidence_against: Vec<yunxi_core::ObservationId> =
                serde_json::from_value(row.try_get("evidence_against")?)
                    .map_err(json_error("evidence_against"))?;
            for id in evidence_for {
                hypothesis.add_evidence(id, true, hypothesis.updated_at())?;
            }
            for id in evidence_against {
                hypothesis.add_evidence(id, false, hypothesis.updated_at())?;
            }
            hypotheses.push(hypothesis);
        }
        let mut scenes = Vec::new();
        let rows = query("SELECT * FROM yunxi_world_scenes")
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let scene = SocialSceneState::restore(
                ConversationId::from_uuid(row.try_get("conversation_id")?),
                serde_json::from_value(row.try_get("participants")?)
                    .map_err(json_error("scene participants"))?,
                serde_json::from_value(row.try_get("floor")?).map_err(json_error("scene floor"))?,
                serde_json::from_value(row.try_get("recent_speakers")?)
                    .map_err(json_error("scene recent_speakers"))?,
                row.try_get("bot_addressed")?,
                row.try_get::<f32, _>("activity_level")?,
                row.try_get::<f32, _>("interruption_cost")?,
                decode_scene_kind(row.try_get::<String, _>("scene_kind")?),
                row.try_get::<i64, _>("conversation_version")? as u64,
                row.try_get("updated_at")?,
            )?;
            scenes.push(scene);
        }
        let mut uncertainties = Vec::new();
        let rows = query("SELECT * FROM yunxi_world_uncertainties")
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            uncertainties.push(WorldUncertainty::new(
                yunxi_core::UncertaintyId::from_uuid(row.try_get("id")?),
                decode_uncertainty_type(row.try_get::<String, _>("uncertainty_type")?),
                decode_scope(
                    row.try_get::<String, _>("scope_kind")?.as_str(),
                    row.try_get::<Option<uuid::Uuid>, _>("scope_id")?,
                )?,
                row.try_get::<String, _>("note")?,
                row.try_get("observed_at")?,
                row.try_get("expires_at")?,
            )?);
        }
        let version: u64 = query("SELECT value FROM yunxi_world_meta WHERE key = 'version'")
            .fetch_optional(&self.pool)
            .await?
            .map(|row| {
                let value: serde_json::Value = row.get("value");
                value
                    .get("version")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1)
            })
            .unwrap_or(1);
        let mut causal = Vec::new();
        let rows = query("SELECT * FROM yunxi_world_causal")
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let relation = yunxi_core::CausalRelation::new(
                yunxi_core::CausalRelationId::from_uuid(row.try_get("id")?),
                yunxi_core::WorldPattern::new(
                    decode_pattern_kind(row.try_get::<String, _>("cause_kind")?),
                    row.try_get::<String, _>("cause_label")?,
                )?,
                yunxi_core::WorldPattern::new(
                    decode_pattern_kind(row.try_get::<String, _>("effect_kind")?),
                    row.try_get::<String, _>("effect_label")?,
                )?,
                row.try_get::<f32, _>("strength")?,
                row.try_get::<f32, _>("confidence")?,
                decode_causal_source(row.try_get::<String, _>("source")?),
                decode_causal_scope(
                    row.try_get::<String, _>("scope_kind")?,
                    row.try_get::<Option<String>, _>("scope_id")?,
                )?,
                row.try_get::<i64, _>("evidence_occurrences")? as u32,
            )?;
            causal.push(relation);
        }
        if observations.is_empty()
            && entities.is_empty()
            && situations.is_empty()
            && hypotheses.is_empty()
            && scenes.is_empty()
            && uncertainties.is_empty()
            && causal.is_empty()
        {
            return Ok(None);
        }
        let mut world = WorldModel::restore_from_parts(
            observations,
            entities,
            situations,
            hypotheses,
            scenes,
            yunxi_core::EnvironmentState::default(),
            Vec::new(),
            uncertainties,
            version,
        )?;
        for relation in causal {
            // Duplicates are impossible after a snapshot restore; errors here
            // mean corrupted rows → fail-soft by skipping that relation.
            if let Err(error) = world.add_causal_relation(relation) {
                eprintln!("[YUNXI_WORLD] causal restore skipped: {error}");
            }
        }
        Ok(Some(world))
    }

    /// Delete person-linked world rows inside the caller's erasure
    /// transaction (v4 §242). Returns the number of rows removed.
    pub(crate) async fn delete_person_domain_rows(
        transaction: &mut Transaction<'_, Postgres>,
        person_id: Option<uuid::Uuid>,
        direct_conversation_ids: &[uuid::Uuid],
    ) -> anyhow::Result<u64> {
        let mut deleted = 0;
        if let Some(person_id) = person_id {
            deleted += query(
                r#"DELETE FROM yunxi_world_observations
                   WHERE (scope_kind = 'person' AND scope_id = $1)
                      OR (scope_kind = 'conversation' AND scope_id = ANY($2::uuid[]))"#,
            )
            .bind(person_id)
            .bind(direct_conversation_ids)
            .execute(&mut **transaction)
            .await?
            .rows_affected();
            deleted += query(
                r#"DELETE FROM yunxi_world_hypotheses
                   WHERE (scope_kind = 'person' AND scope_id = $1)
                      OR (scope_kind = 'conversation' AND scope_id = ANY($2::uuid[]))"#,
            )
            .bind(person_id)
            .bind(direct_conversation_ids)
            .execute(&mut **transaction)
            .await?
            .rows_affected();
            deleted += query(
                r#"DELETE FROM yunxi_world_entities
                   WHERE linked_person = $1 OR linked_conversation = ANY($2::uuid[])"#,
            )
            .bind(person_id)
            .bind(direct_conversation_ids)
            .execute(&mut **transaction)
            .await?
            .rows_affected();
            deleted += query(
                r#"DELETE FROM yunxi_world_situations
                   WHERE conversation_id = ANY($1::uuid[]) OR persons @> $2::jsonb"#,
            )
            .bind(direct_conversation_ids)
            .bind(serde_json::json!([person_id]))
            .execute(&mut **transaction)
            .await?
            .rows_affected();
            deleted += query(
                r#"DELETE FROM yunxi_world_scenes
                   WHERE conversation_id = ANY($1::uuid[])"#,
            )
            .bind(direct_conversation_ids)
            .execute(&mut **transaction)
            .await?
            .rows_affected();
            deleted += query(
                r#"DELETE FROM yunxi_world_uncertainties
                   WHERE (scope_kind = 'person' AND scope_id = $1)
                      OR (scope_kind = 'conversation' AND scope_id = ANY($2::uuid[]))"#,
            )
            .bind(person_id)
            .bind(direct_conversation_ids)
            .execute(&mut **transaction)
            .await?
            .rows_affected();
            deleted += query(
                "DELETE FROM yunxi_world_causal
                 WHERE (scope_kind = 'person_specific' AND scope_id = $1::text)
                    OR (scope_kind = 'conversation_specific' AND scope_id = ANY($2::text[]))",
            )
            .bind(person_id.to_string())
            .bind(
                direct_conversation_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>(),
            )
            .execute(&mut **transaction)
            .await?
            .rows_affected();
        }
        Ok(deleted)
    }

    /// Delete one conversation's world rows (group erasure).
    pub(crate) async fn delete_conversation_domain_rows(
        transaction: &mut Transaction<'_, Postgres>,
        conversation_id: uuid::Uuid,
    ) -> anyhow::Result<u64> {
        let mut deleted = 0;
        for query_sql in [
            "DELETE FROM yunxi_world_observations WHERE scope_kind = 'conversation' AND scope_id = $1",
            "DELETE FROM yunxi_world_hypotheses WHERE scope_kind = 'conversation' AND scope_id = $1",
            "DELETE FROM yunxi_world_entities WHERE linked_conversation = $1",
            "DELETE FROM yunxi_world_situations WHERE conversation_id = $1",
            "DELETE FROM yunxi_world_scenes WHERE conversation_id = $1",
            "DELETE FROM yunxi_world_uncertainties WHERE scope_kind = 'conversation' AND scope_id = $1",
        ] {
            let rows = query(query_sql)
                .bind(conversation_id)
                .execute(&mut **transaction)
                .await?;
            deleted += rows.rows_affected();
        }
        deleted += query(
            "DELETE FROM yunxi_world_causal
             WHERE scope_kind = 'conversation_specific' AND scope_id = $1::text",
        )
        .bind(conversation_id.to_string())
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        Ok(deleted)
    }
}

// ---- encoding helpers (label round-trips with the storage row) ----

fn encode_scope(scope: WorldScope) -> (&'static str, Option<uuid::Uuid>) {
    match scope {
        WorldScope::Global => (SCOPE_GLOBAL, None),
        WorldScope::Person { person_id } => (SCOPE_PERSON, Some(person_id.into_uuid())),
        WorldScope::Conversation { conversation_id } => {
            (SCOPE_CONVERSATION, Some(conversation_id.into_uuid()))
        }
    }
}

fn decode_scope(kind: &str, scope_id: Option<uuid::Uuid>) -> anyhow::Result<WorldScope> {
    match (kind, scope_id) {
        (SCOPE_GLOBAL, None) => Ok(WorldScope::Global),
        (SCOPE_PERSON, Some(id)) => Ok(WorldScope::Person {
            person_id: PersonId::from_uuid(id),
        }),
        (SCOPE_CONVERSATION, Some(id)) => Ok(WorldScope::Conversation {
            conversation_id: ConversationId::from_uuid(id),
        }),
        _ => anyhow::bail!("无法解码 World Model scope: {kind:?} {scope_id:?}"),
    }
}

fn enum_label<T: std::fmt::Debug>(value: T) -> String {
    format!("{value:?}").to_lowercase()
}

fn decode_observation_kind(label: String) -> ObservationKind {
    match label.as_str() {
        "messagereceived" => ObservationKind::MessageReceived,
        "toolresult" => ObservationKind::ToolResult,
        "actionresult" => ObservationKind::ActionResult,
        "hoststate" => ObservationKind::HostState,
        "systemstate" => ObservationKind::SystemState,
        "conversationevent" => ObservationKind::ConversationEvent,
        _ => ObservationKind::SystemState,
    }
}

fn decode_observation_source(label: String) -> ObservationSource {
    match label.as_str() {
        "directuserstatement" => ObservationSource::DirectUserStatement,
        "toolresult" => ObservationSource::ToolResult,
        "platformevent" => ObservationSource::PlatformEvent,
        "systemstate" => ObservationSource::SystemState,
        "modelextraction" => ObservationSource::ModelExtraction,
        "derivedobservation" => ObservationSource::DerivedObservation,
        _ => ObservationSource::DerivedObservation,
    }
}

fn entity_kind_label(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Person => "person",
        EntityKind::Conversation => "conversation",
        EntityKind::Host => "host",
        EntityKind::Tool => "tool",
        EntityKind::Place => "place",
        EntityKind::Topic => "topic",
        EntityKind::Resource => "resource",
        EntityKind::ExternalService => "external_service",
        EntityKind::GoalContext => "goal_context",
        EntityKind::Unknown => "unknown",
    }
}

fn decode_entity_kind(label: String) -> EntityKind {
    match label.as_str() {
        "person" => EntityKind::Person,
        "conversation" => EntityKind::Conversation,
        "host" => EntityKind::Host,
        "tool" => EntityKind::Tool,
        "place" => EntityKind::Place,
        "topic" => EntityKind::Topic,
        "resource" => EntityKind::Resource,
        "external_service" => EntityKind::ExternalService,
        "goal_context" => EntityKind::GoalContext,
        _ => EntityKind::Unknown,
    }
}

fn situation_kind_label(kind: SituationKind) -> &'static str {
    match kind {
        SituationKind::FutureEvent => "future_event",
        SituationKind::ToolTask => "tool_task",
        SituationKind::AgentTask => "agent_task",
        SituationKind::BuildTask => "build_task",
        SituationKind::ConversationState => "conversation_state",
        SituationKind::Unknown => "unknown",
    }
}

fn decode_situation_kind(label: String) -> SituationKind {
    match label.as_str() {
        "future_event" => SituationKind::FutureEvent,
        "tool_task" => SituationKind::ToolTask,
        "agent_task" => SituationKind::AgentTask,
        "build_task" => SituationKind::BuildTask,
        "conversation_state" => SituationKind::ConversationState,
        _ => SituationKind::Unknown,
    }
}

fn situation_state_label(state: SituationState) -> &'static str {
    match state {
        SituationState::Planned => "planned",
        SituationState::InProgress => "in_progress",
        SituationState::OutcomeUnknown => "outcome_unknown",
        SituationState::Completed => "completed",
        SituationState::Failed => "failed",
        SituationState::Expired => "expired",
        SituationState::Unknown => "unknown",
    }
}

fn decode_situation_state(label: String) -> SituationState {
    match label.as_str() {
        "planned" => SituationState::Planned,
        "in_progress" => SituationState::InProgress,
        "outcome_unknown" => SituationState::OutcomeUnknown,
        "completed" => SituationState::Completed,
        "failed" => SituationState::Failed,
        "expired" => SituationState::Expired,
        _ => SituationState::Unknown,
    }
}

fn hypothesis_status_label(status: HypothesisStatus) -> &'static str {
    match status {
        HypothesisStatus::Active => "active",
        HypothesisStatus::Supported => "supported",
        HypothesisStatus::Rejected => "rejected",
        HypothesisStatus::Superseded => "superseded",
        HypothesisStatus::Expired => "expired",
        HypothesisStatus::Unknown => "unknown",
    }
}

fn scene_kind_label(kind: SocialSceneKind) -> &'static str {
    match kind {
        SocialSceneKind::DirectConversation => "direct_conversation",
        SocialSceneKind::GroupDiscussion => "group_discussion",
        SocialSceneKind::RapidGroupChat => "rapid_group_chat",
        SocialSceneKind::IdleGroup => "idle_group",
        SocialSceneKind::TaskConversation => "task_conversation",
        SocialSceneKind::Unknown => "unknown",
    }
}

fn decode_scene_kind(label: String) -> SocialSceneKind {
    match label.as_str() {
        "direct_conversation" => SocialSceneKind::DirectConversation,
        "group_discussion" => SocialSceneKind::GroupDiscussion,
        "rapid_group_chat" => SocialSceneKind::RapidGroupChat,
        "idle_group" => SocialSceneKind::IdleGroup,
        "task_conversation" => SocialSceneKind::TaskConversation,
        _ => SocialSceneKind::Unknown,
    }
}

fn uncertainty_type_label(kind: UncertaintyType) -> &'static str {
    match kind {
        UncertaintyType::StateUnknown => "state_unknown",
        UncertaintyType::TemporalUnknown => "temporal_unknown",
        UncertaintyType::SourceConflict => "source_conflict",
        UncertaintyType::StaleState => "stale_state",
        UncertaintyType::InsufficientEvidence => "insufficient_evidence",
        UncertaintyType::PredictionUncertain => "prediction_uncertain",
    }
}

fn pattern_kind_label(kind: yunxi_core::PatternKind) -> &'static str {
    match kind {
        yunxi_core::PatternKind::Tool => "tool",
        yunxi_core::PatternKind::Host => "host",
        yunxi_core::PatternKind::Environment => "environment",
        yunxi_core::PatternKind::User => "user",
        yunxi_core::PatternKind::Situation => "situation",
        yunxi_core::PatternKind::Unknown => "unknown",
    }
}

fn decode_pattern_kind(label: String) -> yunxi_core::PatternKind {
    match label.as_str() {
        "tool" => yunxi_core::PatternKind::Tool,
        "host" => yunxi_core::PatternKind::Host,
        "environment" => yunxi_core::PatternKind::Environment,
        "user" => yunxi_core::PatternKind::User,
        "situation" => yunxi_core::PatternKind::Situation,
        _ => yunxi_core::PatternKind::Unknown,
    }
}

fn causal_source_label(source: yunxi_core::CausalSource) -> &'static str {
    match source {
        yunxi_core::CausalSource::Seed => "seed",
        yunxi_core::CausalSource::ObservedRepeatedPattern => "observed_repeated_pattern",
        yunxi_core::CausalSource::ToolBehavior => "tool_behavior",
        yunxi_core::CausalSource::Reflection => "reflection",
        yunxi_core::CausalSource::DomainRule => "domain_rule",
    }
}

fn decode_causal_source(label: String) -> yunxi_core::CausalSource {
    match label.as_str() {
        "seed" => yunxi_core::CausalSource::Seed,
        "observed_repeated_pattern" => yunxi_core::CausalSource::ObservedRepeatedPattern,
        "tool_behavior" => yunxi_core::CausalSource::ToolBehavior,
        "reflection" => yunxi_core::CausalSource::Reflection,
        "domain_rule" => yunxi_core::CausalSource::DomainRule,
        _ => yunxi_core::CausalSource::ObservedRepeatedPattern,
    }
}

fn encode_causal_scope(scope: yunxi_core::CausalScope) -> (&'static str, Option<String>) {
    use yunxi_core::CausalScope;
    match scope {
        CausalScope::Global => ("global", None),
        CausalScope::ToolSpecific { tool } => ("tool_specific", Some(tool)),
        CausalScope::PersonSpecific { person_id } => {
            ("person_specific", Some(person_id.into_uuid().to_string()))
        }
        CausalScope::ConversationSpecific { conversation_id } => (
            "conversation_specific",
            Some(conversation_id.into_uuid().to_string()),
        ),
        CausalScope::HostSpecific { host } => ("host_specific", Some(host.as_str().to_owned())),
    }
}

fn decode_causal_scope(
    kind: String,
    scope_id: Option<String>,
) -> anyhow::Result<yunxi_core::CausalScope> {
    use yunxi_core::CausalScope;
    match (kind.as_str(), scope_id.as_deref()) {
        ("global", None) => Ok(CausalScope::Global),
        ("tool_specific", Some(tool)) => Ok(CausalScope::ToolSpecific {
            tool: tool.to_owned(),
        }),
        ("person_specific", Some(id)) => Ok(CausalScope::PersonSpecific {
            person_id: PersonId::from_uuid(uuid::Uuid::parse_str(id)?),
        }),
        ("conversation_specific", Some(id)) => Ok(CausalScope::ConversationSpecific {
            conversation_id: ConversationId::from_uuid(uuid::Uuid::parse_str(id)?),
        }),
        ("host_specific", Some(host)) => Ok(CausalScope::HostSpecific {
            host: yunxi_core::HostId::new(host.to_owned())?,
        }),
        _ => anyhow::bail!("无法解码 causal scope: {kind:?} {scope_id:?}"),
    }
}

fn decode_uncertainty_type(label: String) -> UncertaintyType {
    match label.as_str() {
        "state_unknown" => UncertaintyType::StateUnknown,
        "temporal_unknown" => UncertaintyType::TemporalUnknown,
        "source_conflict" => UncertaintyType::SourceConflict,
        "stale_state" => UncertaintyType::StaleState,
        "insufficient_evidence" => UncertaintyType::InsufficientEvidence,
        "prediction_uncertain" => UncertaintyType::PredictionUncertain,
        _ => UncertaintyType::StateUnknown,
    }
}

fn json_error(context: &'static str) -> impl Fn(serde_json::Error) -> anyhow::Error {
    move |error| anyhow::anyhow!("{context} JSON 解码失败: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_encoding_roundtrips() {
        let person = PersonId::new();
        let conversation = ConversationId::new();
        for scope in [
            WorldScope::Global,
            WorldScope::Person { person_id: person },
            WorldScope::Conversation {
                conversation_id: conversation,
            },
        ] {
            let (kind, id) = encode_scope(scope);
            assert_eq!(decode_scope(kind, id).expect("decodes"), scope);
        }
        assert!(decode_scope("bogus", None).is_err());
    }

    #[test]
    fn causal_scope_encoding_roundtrips() {
        let person = PersonId::new();
        let conversation = ConversationId::new();
        for scope in [
            yunxi_core::CausalScope::Global,
            yunxi_core::CausalScope::ToolSpecific {
                tool: "web_fetch".to_owned(),
            },
            yunxi_core::CausalScope::PersonSpecific { person_id: person },
            yunxi_core::CausalScope::ConversationSpecific {
                conversation_id: conversation,
            },
            yunxi_core::CausalScope::HostSpecific {
                host: yunxi_core::HostId::new("qq").expect("host"),
            },
        ] {
            let (kind, id) = encode_causal_scope(scope.clone());
            assert_eq!(
                decode_causal_scope(kind.to_owned(), id).expect("decodes"),
                scope
            );
        }
        assert!(decode_causal_scope("bogus".to_owned(), None).is_err());
    }

    #[test]
    fn enum_labels_decode_to_distinct_kinds() {
        for kind in [
            ObservationKind::MessageReceived,
            ObservationKind::ToolResult,
            ObservationKind::ActionResult,
            ObservationKind::HostState,
            ObservationKind::SystemState,
            ObservationKind::ConversationEvent,
        ] {
            assert_eq!(decode_observation_kind(enum_label(kind)), kind);
        }
        for kind in [
            EntityKind::Person,
            EntityKind::Conversation,
            EntityKind::Host,
            EntityKind::Tool,
            EntityKind::Place,
            EntityKind::Topic,
            EntityKind::Resource,
            EntityKind::ExternalService,
            EntityKind::GoalContext,
            EntityKind::Unknown,
        ] {
            assert_eq!(decode_entity_kind(entity_kind_label(kind).to_owned()), kind);
        }
        for state in [
            SituationState::Planned,
            SituationState::InProgress,
            SituationState::OutcomeUnknown,
            SituationState::Completed,
            SituationState::Failed,
            SituationState::Expired,
            SituationState::Unknown,
        ] {
            assert_eq!(
                decode_situation_state(situation_state_label(state).to_owned()),
                state
            );
        }
        for scene in [
            SocialSceneKind::DirectConversation,
            SocialSceneKind::GroupDiscussion,
            SocialSceneKind::RapidGroupChat,
            SocialSceneKind::IdleGroup,
            SocialSceneKind::TaskConversation,
            SocialSceneKind::Unknown,
        ] {
            assert_eq!(decode_scene_kind(scene_kind_label(scene).to_owned()), scene);
        }
        for status in [
            HypothesisStatus::Active,
            HypothesisStatus::Supported,
            HypothesisStatus::Rejected,
            HypothesisStatus::Superseded,
            HypothesisStatus::Expired,
            HypothesisStatus::Unknown,
        ] {
            assert_eq!(
                decode_hypothesis_status_for_test(hypothesis_status_label(status).to_owned()),
                status
            );
        }
    }

    fn decode_hypothesis_status_for_test(label: String) -> HypothesisStatus {
        match label.as_str() {
            "active" => HypothesisStatus::Active,
            "supported" => HypothesisStatus::Supported,
            "rejected" => HypothesisStatus::Rejected,
            "superseded" => HypothesisStatus::Superseded,
            "expired" => HypothesisStatus::Expired,
            _ => HypothesisStatus::Unknown,
        }
    }
}
