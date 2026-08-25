//! Optional, platform-neutral persistence adapters for the standalone CLI.
//!
//! Persistence remains a host concern: `yunxi-core` only sees its store
//! traits, while this module owns the bounded JSON snapshot and filesystem
//! operations. Every mutation is serialized under one lock and publishes a
//! complete replacement snapshot, so the four stores cannot drift apart.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use yunxi_core::{
    AffectState, AffectStore, AffectStoreError, AffectStoreFuture, ConversationId, Memory,
    MemoryDraft, MemoryId, MemoryKind, MemoryQuery, MemoryScope, MemoryStore, MemoryStoreError,
    MemoryStoreFuture, OpenLoop, OpenLoopDraft, OpenLoopId, OpenLoopOwner, OpenLoopStatus,
    OpenLoopStore, OpenLoopStoreError, OpenLoopStoreFuture, PersonId, RelationState, RelationStore,
    RelationStoreError, RelationStoreFuture,
};

const STATE_SCHEMA_VERSION: u32 = 1;
const CLAIM_LEASE: Duration = Duration::from_secs(15 * 60);

/// Maximum encoded size accepted for one CLI state snapshot.
pub const MAX_CLI_STATE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum memories retained across every scope.
pub const MAX_CLI_MEMORIES: usize = 256;
/// Maximum memories retained for one exact Core scope.
pub const MAX_CLI_MEMORIES_PER_SCOPE: usize = 64;
/// Maximum open loops retained, including terminal history.
pub const MAX_CLI_OPEN_LOOPS: usize = 256;
/// Maximum active open loops for one exact owner.
pub const MAX_CLI_OPEN_LOOPS_PER_OWNER: usize = 64;
/// Maximum distinct people retained by each social-state store.
pub const MAX_CLI_PEOPLE: usize = 256;
const MAX_OPEN_LOOP_OPERATION_LIMIT: usize = 128;

#[derive(Debug, Error)]
pub enum CliStateError {
    #[error("CLI state I/O failed")]
    Io(#[source] io::Error),
    #[error("CLI state JSON encoding failed")]
    Encode(#[source] serde_json::Error),
    #[error("CLI state JSON is invalid")]
    Decode(#[source] serde_json::Error),
    #[error("CLI state file is {length} bytes, above maximum {maximum}")]
    FileTooLarge { length: u64, maximum: usize },
    #[error("unsupported CLI state schema version {version}")]
    UnsupportedVersion { version: u32 },
    #[error("CLI state is invalid: {reason}")]
    Invalid { reason: String },
    #[error("CLI state capacity `{kind}` is exhausted at {maximum}")]
    Capacity { kind: &'static str, maximum: usize },
    #[error("open loop {id} was not found in CLI state")]
    OpenLoopNotFound { id: OpenLoopId },
    #[error("invalid CLI open-loop transition from {from} to {to}")]
    OpenLoopTransition {
        from: OpenLoopStatus,
        to: OpenLoopStatus,
    },
    #[error("CLI state lock is poisoned")]
    LockPoisoned,
}

impl From<io::Error> for CliStateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliStateStats {
    pub memories: usize,
    pub open_loops: usize,
    pub affects: usize,
    pub relations: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AffectEntry {
    person_id: PersonId,
    state: AffectState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateSnapshot {
    schema_version: u32,
    generation: u64,
    person_id: PersonId,
    conversation_id: ConversationId,
    memories: Vec<Memory>,
    open_loops: Vec<OpenLoop>,
    affects: Vec<AffectEntry>,
    relations: Vec<RelationState>,
}

impl StateSnapshot {
    fn new(person_id: PersonId, conversation_id: ConversationId) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            generation: 0,
            person_id,
            conversation_id,
            memories: Vec::new(),
            open_loops: Vec::new(),
            affects: Vec::new(),
            relations: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), CliStateError> {
        if self.schema_version != STATE_SCHEMA_VERSION {
            return Err(CliStateError::UnsupportedVersion {
                version: self.schema_version,
            });
        }
        validate_capacity("memories", self.memories.len(), MAX_CLI_MEMORIES)?;
        validate_capacity("open_loops", self.open_loops.len(), MAX_CLI_OPEN_LOOPS)?;
        validate_capacity("affects", self.affects.len(), MAX_CLI_PEOPLE)?;
        validate_capacity("relations", self.relations.len(), MAX_CLI_PEOPLE)?;

        let mut memory_ids = HashSet::with_capacity(self.memories.len());
        let mut memories_by_scope = HashMap::<MemoryScope, usize>::new();
        for memory in &self.memories {
            if !memory_ids.insert(memory.id()) {
                return Err(invalid_state(format!(
                    "duplicate memory id {}",
                    memory.id()
                )));
            }
            let count = memories_by_scope.entry(memory.scope()).or_default();
            *count += 1;
            validate_capacity("memories per scope", *count, MAX_CLI_MEMORIES_PER_SCOPE)?;
        }

        let mut open_loop_ids = HashSet::with_capacity(self.open_loops.len());
        let mut active_dedupe = HashSet::new();
        let mut active_by_owner = HashMap::<OpenLoopOwner, usize>::new();
        for item in &self.open_loops {
            if !open_loop_ids.insert(item.id()) {
                return Err(invalid_state(format!(
                    "duplicate open-loop id {}",
                    item.id()
                )));
            }
            if item.is_active() {
                let count = active_by_owner.entry(item.owner()).or_default();
                *count += 1;
                validate_capacity(
                    "active open loops per owner",
                    *count,
                    MAX_CLI_OPEN_LOOPS_PER_OWNER,
                )?;
            }
            if item.is_active()
                && let Some(key) = item.dedupe_key()
                && !active_dedupe.insert((item.owner(), key.to_owned()))
            {
                return Err(invalid_state(format!(
                    "duplicate active open-loop dedupe key `{key}`"
                )));
            }
        }

        let mut affect_people = HashSet::with_capacity(self.affects.len());
        for entry in &self.affects {
            entry
                .state
                .validate()
                .map_err(|error| invalid_state(error.to_string()))?;
            if !affect_people.insert(entry.person_id) {
                return Err(invalid_state(format!(
                    "duplicate affect person {}",
                    entry.person_id
                )));
            }
        }

        let mut relation_people = HashSet::with_capacity(self.relations.len());
        for relation in &self.relations {
            relation
                .validate()
                .map_err(|error| invalid_state(error.to_string()))?;
            if !relation_people.insert(relation.person_id) {
                return Err(invalid_state(format!(
                    "duplicate relation person {}",
                    relation.person_id
                )));
            }
        }
        Ok(())
    }
}

/// Shared implementation of the four durable Core service ports used by the
/// standalone CLI. `in_memory_for` keeps persistence optional while exposing
/// the exact same behavior to the runtime and fake model.
#[derive(Debug)]
pub struct CliCoreState {
    path: Option<PathBuf>,
    person_id: PersonId,
    conversation_id: ConversationId,
    snapshot: Mutex<StateSnapshot>,
}

impl CliCoreState {
    #[must_use]
    pub fn in_memory_for(person_id: PersonId, conversation_id: ConversationId) -> Self {
        Self {
            path: None,
            person_id,
            conversation_id,
            snapshot: Mutex::new(StateSnapshot::new(person_id, conversation_id)),
        }
    }

    /// Opens or creates one bounded JSON snapshot. The snapshot also owns the
    /// stable local Core identities, allowing context to survive CLI restarts.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CliStateError> {
        let path = path.as_ref().to_path_buf();
        let snapshot = match read_snapshot(&path) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                let snapshot = StateSnapshot::new(PersonId::new(), ConversationId::new());
                persist_snapshot(&path, &snapshot)?;
                snapshot
            }
            Err(error) => return Err(error),
        };
        snapshot.validate()?;
        Ok(Self {
            person_id: snapshot.person_id,
            conversation_id: snapshot.conversation_id,
            path: Some(path),
            snapshot: Mutex::new(snapshot),
        })
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub const fn person_id(&self) -> PersonId {
        self.person_id
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    pub fn stats(&self) -> Result<CliStateStats, CliStateError> {
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| CliStateError::LockPoisoned)?;
        Ok(CliStateStats {
            memories: snapshot.memories.len(),
            open_loops: snapshot.open_loops.len(),
            affects: snapshot.affects.len(),
            relations: snapshot.relations.len(),
        })
    }

    /// Records one observed CLI message in conversation-scoped memory before
    /// planning, so the next turn can hydrate it through `MemoryStore`.
    pub fn remember_message(
        &self,
        conversation_id: ConversationId,
        content: &str,
        occurred_at: DateTime<Utc>,
    ) -> Result<Memory, CliStateError> {
        let draft = MemoryDraft::new(
            MemoryScope::Conversation(conversation_id),
            MemoryKind::Conversation,
            content,
            occurred_at,
        )
        .and_then(|draft| draft.with_tags(["cli_ingress"]))
        .map_err(|error| invalid_state(error.to_string()))?;
        self.remember_inner(&draft)
    }

    fn transact<T>(
        &self,
        operation: impl FnOnce(&mut StateSnapshot) -> Result<T, CliStateError>,
    ) -> Result<T, CliStateError> {
        let mut current = self
            .snapshot
            .lock()
            .map_err(|_| CliStateError::LockPoisoned)?;
        let mut next = current.clone();
        let output = operation(&mut next)?;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid_state("snapshot generation exhausted"))?;
        next.validate()?;
        if let Some(path) = &self.path {
            persist_snapshot(path, &next)?;
        }
        *current = next;
        Ok(output)
    }

    fn remember_inner(&self, draft: &MemoryDraft) -> Result<Memory, CliStateError> {
        draft
            .validate()
            .map_err(|error| invalid_state(error.to_string()))?;
        let memory = Memory::from_draft(MemoryId::new(), draft, Utc::now())
            .map_err(|error| invalid_state(error.to_string()))?;
        self.transact(|snapshot| {
            while snapshot
                .memories
                .iter()
                .filter(|candidate| candidate.scope() == draft.scope())
                .count()
                >= MAX_CLI_MEMORIES_PER_SCOPE
            {
                remove_oldest_memory(&mut snapshot.memories, Some(draft.scope()));
            }
            while snapshot.memories.len() >= MAX_CLI_MEMORIES {
                remove_oldest_memory(&mut snapshot.memories, None);
            }
            snapshot.memories.push(memory.clone());
            Ok(memory.clone())
        })
    }

    fn recall_inner(&self, query: &MemoryQuery) -> Result<Vec<Memory>, CliStateError> {
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| CliStateError::LockPoisoned)?;
        let needle = query.text().to_lowercase();
        let mut matches = snapshot
            .memories
            .iter()
            .filter(|memory| memory.scope() == query.scope())
            .filter(|memory| {
                query
                    .min_importance()
                    .is_none_or(|minimum| memory.importance() >= minimum)
            })
            .filter(|memory| needle.is_empty() || memory.content().to_lowercase().contains(&needle))
            .cloned()
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .importance()
                .cmp(&left.importance())
                .then_with(|| right.occurred_at().cmp(&left.occurred_at()))
                .then_with(|| right.id().cmp(&left.id()))
        });
        matches.truncate(query.limit());
        Ok(matches)
    }

    fn forget_inner(&self, scope: MemoryScope, id: MemoryId) -> Result<bool, CliStateError> {
        self.transact(|snapshot| {
            let before = snapshot.memories.len();
            snapshot
                .memories
                .retain(|memory| memory.id() != id || memory.scope() != scope);
            Ok(snapshot.memories.len() != before)
        })
    }

    fn affect_inner(&self, person_id: PersonId) -> Result<AffectState, CliStateError> {
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| CliStateError::LockPoisoned)?;
        Ok(snapshot
            .affects
            .iter()
            .find(|entry| entry.person_id == person_id)
            .map_or_else(AffectState::default, |entry| entry.state))
    }

    fn set_affect_inner(
        &self,
        person_id: PersonId,
        state: AffectState,
    ) -> Result<AffectState, CliStateError> {
        state
            .validate()
            .map_err(|error| invalid_state(error.to_string()))?;
        self.transact(|snapshot| {
            if let Some(entry) = snapshot
                .affects
                .iter_mut()
                .find(|entry| entry.person_id == person_id)
            {
                entry.state = state;
            } else {
                if snapshot.affects.len() >= MAX_CLI_PEOPLE {
                    return Err(CliStateError::Capacity {
                        kind: "affects",
                        maximum: MAX_CLI_PEOPLE,
                    });
                }
                snapshot.affects.push(AffectEntry { person_id, state });
            }
            Ok(state)
        })
    }

    fn relation_inner(&self, person_id: PersonId) -> Result<Option<RelationState>, CliStateError> {
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| CliStateError::LockPoisoned)?;
        Ok(snapshot
            .relations
            .iter()
            .find(|relation| relation.person_id == person_id)
            .copied())
    }

    fn set_relation_inner(&self, state: RelationState) -> Result<RelationState, CliStateError> {
        state
            .validate()
            .map_err(|error| invalid_state(error.to_string()))?;
        self.transact(|snapshot| {
            if let Some(existing) = snapshot
                .relations
                .iter_mut()
                .find(|relation| relation.person_id == state.person_id)
            {
                *existing = state;
            } else {
                if snapshot.relations.len() >= MAX_CLI_PEOPLE {
                    return Err(CliStateError::Capacity {
                        kind: "relations",
                        maximum: MAX_CLI_PEOPLE,
                    });
                }
                snapshot.relations.push(state);
            }
            Ok(state)
        })
    }

    fn create_open_loop_inner(&self, draft: &OpenLoopDraft) -> Result<OpenLoop, CliStateError> {
        draft
            .validate()
            .map_err(|error| invalid_state(error.to_string()))?;
        self.transact(|snapshot| {
            if let Some(key) = draft.dedupe_key()
                && let Some(existing) = snapshot.open_loops.iter().find(|item| {
                    item.owner() == draft.owner()
                        && item.is_active()
                        && item.dedupe_key() == Some(key)
                })
            {
                return Ok(existing.clone());
            }
            let active_for_owner = snapshot
                .open_loops
                .iter()
                .filter(|item| item.owner() == draft.owner() && item.is_active())
                .count();
            if active_for_owner >= MAX_CLI_OPEN_LOOPS_PER_OWNER {
                return Err(CliStateError::Capacity {
                    kind: "active open loops per owner",
                    maximum: MAX_CLI_OPEN_LOOPS_PER_OWNER,
                });
            }
            if snapshot.open_loops.len() >= MAX_CLI_OPEN_LOOPS {
                let oldest_terminal = snapshot
                    .open_loops
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.status().is_terminal())
                    .min_by_key(|(_, item)| (item.updated_at(), item.id()))
                    .map(|(index, _)| index)
                    .ok_or(CliStateError::Capacity {
                        kind: "open loops",
                        maximum: MAX_CLI_OPEN_LOOPS,
                    })?;
                snapshot.open_loops.remove(oldest_terminal);
            }
            let item = OpenLoop::from_draft(OpenLoopId::new(), draft, Utc::now())
                .map_err(|error| invalid_state(error.to_string()))?;
            snapshot.open_loops.push(item.clone());
            Ok(item)
        })
    }

    fn get_open_loop_inner(&self, id: OpenLoopId) -> Result<Option<OpenLoop>, CliStateError> {
        self.transact(|snapshot| {
            let Some(index) = snapshot.open_loops.iter().position(|item| item.id() == id) else {
                return Ok(None);
            };
            expire_if_needed(&mut snapshot.open_loops[index], Utc::now())?;
            Ok(Some(snapshot.open_loops[index].clone()))
        })
    }

    fn list_open_loops_inner(
        &self,
        owner: OpenLoopOwner,
        limit: usize,
    ) -> Result<Vec<OpenLoop>, CliStateError> {
        validate_operation_limit("open-loop list", limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.transact(|snapshot| {
            let now = Utc::now();
            for item in snapshot
                .open_loops
                .iter_mut()
                .filter(|item| item.owner() == owner)
            {
                expire_if_needed(item, now)?;
            }
            let mut items = snapshot
                .open_loops
                .iter()
                .filter(|item| item.owner() == owner && item.is_active())
                .cloned()
                .collect::<Vec<_>>();
            items.sort_by_key(|item| {
                (
                    item.due_at().is_none(),
                    item.due_at(),
                    item.created_at(),
                    item.id(),
                )
            });
            items.truncate(limit);
            Ok(items)
        })
    }

    fn claim_due_inner(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<OpenLoop>, CliStateError> {
        validate_operation_limit("open-loop claim", limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.transact(|snapshot| {
            for item in &mut snapshot.open_loops {
                expire_if_needed(item, now)?;
            }
            let mut candidates = snapshot
                .open_loops
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    item.status() == OpenLoopStatus::Open
                        && item.due_at().is_some_and(|due_at| due_at <= now)
                })
                .map(|(index, item)| (index, item.due_at(), item.id()))
                .collect::<Vec<_>>();
            candidates.sort_by_key(|(_, due_at, id)| (*due_at, *id));
            candidates.truncate(limit);
            let mut claimed = Vec::with_capacity(candidates.len());
            for (index, _, _) in candidates {
                let transitioned = snapshot.open_loops[index]
                    .clone()
                    .transition(OpenLoopStatus::Triggered, now)
                    .map_err(|error| invalid_state(error.to_string()))?;
                snapshot.open_loops[index] = transitioned.clone();
                claimed.push(transitioned);
            }
            Ok(claimed)
        })
    }

    fn defer_open_loop_inner(
        &self,
        id: OpenLoopId,
        due_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<OpenLoop, CliStateError> {
        self.transact(|snapshot| {
            let index = snapshot
                .open_loops
                .iter()
                .position(|item| item.id() == id)
                .ok_or(CliStateError::OpenLoopNotFound { id })?;
            let item = snapshot.open_loops[index].clone();
            if item.status().is_terminal() {
                return Err(CliStateError::OpenLoopTransition {
                    from: item.status(),
                    to: OpenLoopStatus::Open,
                });
            }
            if item
                .expires_at()
                .is_some_and(|expires_at| expires_at <= now)
            {
                let expired = item
                    .transition(OpenLoopStatus::Expired, now)
                    .map_err(|error| invalid_state(error.to_string()))?;
                snapshot.open_loops[index] = expired.clone();
                return Ok(expired);
            }
            if let (Some(due_at), Some(expires_at)) = (due_at, item.expires_at())
                && due_at > expires_at
            {
                return Err(invalid_state("open-loop expiry is before its due time"));
            }
            let reopened = item
                .transition(OpenLoopStatus::Open, now)
                .map_err(|error| invalid_state(error.to_string()))?;
            let restored = OpenLoop::restore(
                reopened.id(),
                reopened.owner(),
                reopened.kind(),
                reopened.summary(),
                reopened.source_message_id(),
                due_at,
                reopened.expires_at(),
                reopened.salience(),
                reopened.status(),
                reopened.created_at(),
                reopened.updated_at(),
                reopened.resolved_at(),
                reopened.triggered_at(),
                reopened.version(),
                reopened.dedupe_key().map(str::to_owned),
            )
            .map_err(|error| invalid_state(error.to_string()))?;
            snapshot.open_loops[index] = restored.clone();
            Ok(restored)
        })
    }

    fn transition_open_loop_inner(
        &self,
        id: OpenLoopId,
        target: OpenLoopStatus,
        now: DateTime<Utc>,
    ) -> Result<OpenLoop, CliStateError> {
        self.transact(|snapshot| {
            let index = snapshot
                .open_loops
                .iter()
                .position(|item| item.id() == id)
                .ok_or(CliStateError::OpenLoopNotFound { id })?;
            let item = snapshot.open_loops[index].clone();
            if item.status() == target {
                return Ok(item);
            }
            let transitioned = item
                .transition(target, now)
                .map_err(open_loop_transition_error)?;
            snapshot.open_loops[index] = transitioned.clone();
            Ok(transitioned)
        })
    }

    fn recover_stale_inner(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<usize, CliStateError> {
        validate_operation_limit("open-loop recovery", limit)?;
        if limit == 0 {
            return Ok(0);
        }
        let stale_before = now
            - ChronoDuration::from_std(CLAIM_LEASE)
                .map_err(|error| invalid_state(error.to_string()))?;
        self.transact(|snapshot| {
            let mut candidates = snapshot
                .open_loops
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    item.status() == OpenLoopStatus::Triggered
                        && item
                            .triggered_at()
                            .is_some_and(|triggered_at| triggered_at <= stale_before)
                })
                .map(|(index, item)| (index, item.triggered_at(), item.id()))
                .collect::<Vec<_>>();
            candidates.sort_by_key(|(_, triggered_at, id)| (*triggered_at, *id));
            candidates.truncate(limit);
            let recovered = candidates.len();
            for (index, _, _) in candidates {
                let target = if snapshot.open_loops[index].is_expired_at(now) {
                    OpenLoopStatus::Expired
                } else {
                    OpenLoopStatus::Open
                };
                snapshot.open_loops[index] = snapshot.open_loops[index]
                    .clone()
                    .transition(target, now)
                    .map_err(|error| invalid_state(error.to_string()))?;
            }
            Ok(recovered)
        })
    }
}

impl MemoryStore for CliCoreState {
    fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
        Box::pin(async move { self.remember_inner(draft).map_err(memory_store_error) })
    }

    fn recall<'a>(&'a self, query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
        Box::pin(async move { self.recall_inner(query).map_err(memory_store_error) })
    }

    fn forget(&self, scope: MemoryScope, id: MemoryId) -> MemoryStoreFuture<'_, bool> {
        Box::pin(async move { self.forget_inner(scope, id).map_err(memory_store_error) })
    }
}

impl AffectStore for CliCoreState {
    fn get<'a>(&'a self, person_id: PersonId) -> AffectStoreFuture<'a, AffectState> {
        Box::pin(async move {
            self.affect_inner(person_id)
                .map_err(AffectStoreError::storage)
        })
    }

    fn set<'a>(
        &'a self,
        person_id: PersonId,
        state: AffectState,
    ) -> AffectStoreFuture<'a, AffectState> {
        Box::pin(async move {
            self.set_affect_inner(person_id, state)
                .map_err(affect_store_error)
        })
    }
}

impl RelationStore for CliCoreState {
    fn get<'a>(&'a self, person_id: PersonId) -> RelationStoreFuture<'a, Option<RelationState>> {
        Box::pin(async move {
            self.relation_inner(person_id)
                .map_err(RelationStoreError::storage)
        })
    }

    fn set<'a>(&'a self, state: RelationState) -> RelationStoreFuture<'a, RelationState> {
        Box::pin(async move { self.set_relation_inner(state).map_err(relation_store_error) })
    }
}

impl OpenLoopStore for CliCoreState {
    fn create<'a>(&'a self, draft: &'a OpenLoopDraft) -> OpenLoopStoreFuture<'a, OpenLoop> {
        Box::pin(async move {
            self.create_open_loop_inner(draft)
                .map_err(|error| open_loop_store_error(error, Some(draft.owner())))
        })
    }

    fn get<'a>(&'a self, id: OpenLoopId) -> OpenLoopStoreFuture<'a, Option<OpenLoop>> {
        Box::pin(async move {
            self.get_open_loop_inner(id)
                .map_err(|error| open_loop_store_error(error, None))
        })
    }

    fn list<'a>(
        &'a self,
        owner: &'a OpenLoopOwner,
        limit: usize,
    ) -> OpenLoopStoreFuture<'a, Vec<OpenLoop>> {
        Box::pin(async move {
            self.list_open_loops_inner(*owner, limit)
                .map_err(|error| open_loop_store_error(error, Some(*owner)))
        })
    }

    fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> OpenLoopStoreFuture<'_, Vec<OpenLoop>> {
        Box::pin(async move {
            self.claim_due_inner(now, limit)
                .map_err(|error| open_loop_store_error(error, None))
        })
    }

    fn defer(
        &self,
        id: OpenLoopId,
        due_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async move {
            self.defer_open_loop_inner(id, due_at, now)
                .map_err(map_open_loop_id_error)
        })
    }

    fn resolve(&self, id: OpenLoopId, now: DateTime<Utc>) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async move {
            self.transition_open_loop_inner(id, OpenLoopStatus::Resolved, now)
                .map_err(map_open_loop_id_error)
        })
    }

    fn cancel(&self, id: OpenLoopId, now: DateTime<Utc>) -> OpenLoopStoreFuture<'_, OpenLoop> {
        Box::pin(async move {
            self.transition_open_loop_inner(id, OpenLoopStatus::Cancelled, now)
                .map_err(map_open_loop_id_error)
        })
    }

    fn recover_stale_triggered(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> OpenLoopStoreFuture<'_, usize> {
        Box::pin(async move {
            self.recover_stale_inner(now, limit)
                .map_err(|error| open_loop_store_error(error, None))
        })
    }

    fn claim_lease(&self) -> Duration {
        CLAIM_LEASE
    }
}

fn read_snapshot(path: &Path) -> Result<Option<StateSnapshot>, CliStateError> {
    let backup = backup_path(path)?;
    if !path.exists() && backup.exists() {
        fs::rename(&backup, path)?;
        sync_parent_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > MAX_CLI_STATE_BYTES as u64 {
        return Err(CliStateError::FileTooLarge {
            length: metadata.len(),
            maximum: MAX_CLI_STATE_BYTES,
        });
    }
    let bytes = fs::read(path)?;
    if bytes.len() > MAX_CLI_STATE_BYTES {
        return Err(CliStateError::FileTooLarge {
            length: bytes.len() as u64,
            maximum: MAX_CLI_STATE_BYTES,
        });
    }
    let snapshot: StateSnapshot = serde_json::from_slice(&bytes).map_err(CliStateError::Decode)?;
    snapshot.validate()?;
    if backup.exists() {
        fs::remove_file(&backup)?;
        sync_parent_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    }
    Ok(Some(snapshot))
}

fn persist_snapshot(path: &Path, snapshot: &StateSnapshot) -> Result<(), CliStateError> {
    snapshot.validate()?;
    let bytes = serde_json::to_vec(snapshot).map_err(CliStateError::Encode)?;
    if bytes.len() > MAX_CLI_STATE_BYTES {
        return Err(CliStateError::FileTooLarge {
            length: bytes.len() as u64,
            maximum: MAX_CLI_STATE_BYTES,
        });
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_state("state path has no file name"))?
        .to_string_lossy();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut last_collision = None;
    for nonce in 0..16_u8 {
        let temporary = parent.join(format!(
            ".{file_name}.tmp-{}-{}-{nonce}",
            std::process::id(),
            snapshot.generation
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        // Unix state can contain personal memory, so never inherit a broad
        // process umask. Other platforms use their native ACL inheritance.
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let write_result = (|| -> io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            replace_snapshot(&temporary, path, parent)
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        return Ok(());
    }
    Err(last_collision
        .unwrap_or_else(|| io::Error::other("temporary state path collision"))
        .into())
}

fn replace_snapshot(temporary: &Path, path: &Path, parent: &Path) -> io::Result<()> {
    match fs::rename(temporary, path) {
        Ok(()) => {
            sync_parent_directory(parent)?;
            Ok(())
        }
        Err(_) if path.exists() => {
            // Windows does not replace an existing destination with rename.
            // Keep the prior complete snapshot at a deterministic backup;
            // `read_snapshot` restores it if the process stops mid-switch.
            let backup = backup_path(path).map_err(io::Error::other)?;
            if backup.exists() {
                fs::remove_file(&backup)?;
            }
            fs::rename(path, &backup)?;
            sync_parent_directory(parent)?;
            if let Err(error) = fs::rename(temporary, path) {
                let _ = fs::rename(&backup, path);
                let _ = sync_parent_directory(parent);
                return Err(error);
            }
            sync_parent_directory(parent)?;
            fs::remove_file(&backup)?;
            sync_parent_directory(parent)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn backup_path(path: &Path) -> Result<PathBuf, CliStateError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_state("state path has no file name"))?
        .to_string_lossy();
    Ok(path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file_name}.backup")))
}

fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    // Rust std does not expose directory fsync on every host. The backup
    // switch remains the portable crash-recovery boundary on those systems.
    let _ = parent;
    Ok(())
}

fn validate_capacity(
    kind: &'static str,
    length: usize,
    maximum: usize,
) -> Result<(), CliStateError> {
    if length > maximum {
        return Err(CliStateError::Capacity { kind, maximum });
    }
    Ok(())
}

fn validate_operation_limit(kind: &'static str, limit: usize) -> Result<(), CliStateError> {
    if limit > MAX_OPEN_LOOP_OPERATION_LIMIT {
        return Err(invalid_state(format!(
            "{kind} limit {limit} exceeds {MAX_OPEN_LOOP_OPERATION_LIMIT}"
        )));
    }
    Ok(())
}

fn invalid_state(reason: impl Into<String>) -> CliStateError {
    CliStateError::Invalid {
        reason: reason.into(),
    }
}

fn remove_oldest_memory(memories: &mut Vec<Memory>, scope: Option<MemoryScope>) {
    let oldest = memories
        .iter()
        .enumerate()
        .filter(|(_, memory)| scope.is_none_or(|scope| memory.scope() == scope))
        .min_by_key(|(_, memory)| (memory.created_at(), memory.id()))
        .map(|(index, _)| index)
        .expect("memory eviction only runs for a non-empty candidate set");
    memories.remove(oldest);
}

fn expire_if_needed(item: &mut OpenLoop, now: DateTime<Utc>) -> Result<(), CliStateError> {
    if item.is_expired_at(now) {
        *item = item
            .clone()
            .transition(OpenLoopStatus::Expired, now)
            .map_err(|error| invalid_state(error.to_string()))?;
    }
    Ok(())
}

fn open_loop_transition_error(error: yunxi_core::OpenLoopValidationError) -> CliStateError {
    match error {
        yunxi_core::OpenLoopValidationError::InvalidTransition { from, to } => {
            CliStateError::OpenLoopTransition { from, to }
        }
        error => invalid_state(error.to_string()),
    }
}

fn memory_store_error(error: CliStateError) -> MemoryStoreError {
    match error {
        CliStateError::Invalid { reason } => MemoryStoreError::InvalidRequest { reason },
        error => MemoryStoreError::storage(error),
    }
}

fn affect_store_error(error: CliStateError) -> AffectStoreError {
    match error {
        CliStateError::Invalid { .. } => AffectStoreError::InvalidState,
        error => AffectStoreError::storage(error),
    }
}

fn relation_store_error(error: CliStateError) -> RelationStoreError {
    match error {
        CliStateError::Invalid { .. } => RelationStoreError::InvalidState,
        error => RelationStoreError::storage(error),
    }
}

fn open_loop_store_error(error: CliStateError, owner: Option<OpenLoopOwner>) -> OpenLoopStoreError {
    match error {
        CliStateError::Capacity { maximum, .. } => OpenLoopStoreError::CapacityExceeded {
            owner: owner.unwrap_or(OpenLoopOwner::Global),
            limit: maximum,
        },
        CliStateError::Invalid { reason } => OpenLoopStoreError::InvalidRequest { reason },
        error => OpenLoopStoreError::storage(error),
    }
}

fn map_open_loop_id_error(error: CliStateError) -> OpenLoopStoreError {
    match error {
        CliStateError::OpenLoopNotFound { id } => OpenLoopStoreError::NotFound { id },
        CliStateError::OpenLoopTransition { from, to } => {
            OpenLoopStoreError::InvalidTransition { from, to }
        }
        error => open_loop_store_error(error, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "yunxi-cli-state-{label}-{}-{nonce}.json",
            std::process::id()
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
    }

    #[test]
    fn snapshot_recovers_all_core_stores_and_stable_ids() {
        let path = unique_path("recover");
        let state = CliCoreState::open(&path).expect("open state");
        let person_id = state.person_id();
        let conversation_id = state.conversation_id();
        let now = Utc::now();
        state
            .remember_message(conversation_id, "persistent memory", now)
            .expect("remember");
        state
            .set_affect_inner(
                person_id,
                AffectState {
                    valence: 0.25,
                    arousal: 0.1,
                    social_energy: 0.8,
                    curiosity: 0.7,
                },
            )
            .expect("set affect");
        let relation = RelationState {
            person_id,
            familiarity: 0.4,
            affinity: 0.2,
            trust: 0.1,
            comfort: 0.3,
            tension: 0.0,
        };
        state.set_relation_inner(relation).expect("set relation");
        state
            .create_open_loop_inner(
                &OpenLoopDraft::new(
                    OpenLoopOwner::Conversation(conversation_id),
                    yunxi_core::OpenLoopKind::FollowUp,
                    "persistent loop",
                )
                .expect("draft"),
            )
            .expect("create loop");
        drop(state);

        let backup = backup_path(&path).expect("backup path");
        fs::rename(&path, &backup).expect("simulate interrupted replacement");

        let reopened = CliCoreState::open(&path).expect("reopen state");
        assert_eq!(reopened.person_id(), person_id);
        assert_eq!(reopened.conversation_id(), conversation_id);
        let query = MemoryQuery::new(MemoryScope::Conversation(conversation_id), "persistent", 10)
            .expect("query");
        assert_eq!(reopened.recall_inner(&query).expect("recall").len(), 1);
        assert_eq!(
            reopened.affect_inner(person_id).expect("affect").valence,
            0.25
        );
        assert_eq!(
            reopened.relation_inner(person_id).expect("relation"),
            Some(relation)
        );
        assert_eq!(
            reopened
                .list_open_loops_inner(OpenLoopOwner::Conversation(conversation_id), 10)
                .expect("list")
                .len(),
            1
        );
        assert!(!backup.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("state metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        cleanup(&path);
    }

    #[test]
    fn memory_and_open_loop_reads_are_scope_isolated() {
        let person_a = PersonId::new();
        let person_b = PersonId::new();
        let conversation = ConversationId::new();
        let state = CliCoreState::in_memory_for(person_a, conversation);
        let now = Utc::now();
        let memory_a = state
            .remember_inner(
                &MemoryDraft::new(
                    MemoryScope::Person(person_a),
                    MemoryKind::Fact,
                    "only a",
                    now,
                )
                .expect("draft a"),
            )
            .expect("remember a");
        state
            .remember_inner(
                &MemoryDraft::new(
                    MemoryScope::Person(person_b),
                    MemoryKind::Fact,
                    "only b",
                    now,
                )
                .expect("draft b"),
            )
            .expect("remember b");
        let query_a = MemoryQuery::new(MemoryScope::Person(person_a), "", 10).expect("query a");
        let query_b = MemoryQuery::new(MemoryScope::Person(person_b), "", 10).expect("query b");
        assert_eq!(state.recall_inner(&query_a).expect("recall a").len(), 1);
        assert_eq!(state.recall_inner(&query_b).expect("recall b").len(), 1);
        assert!(
            !state
                .forget_inner(MemoryScope::Person(person_b), memory_a.id())
                .expect("foreign forget")
        );

        for owner in [
            OpenLoopOwner::Person(person_a),
            OpenLoopOwner::Person(person_b),
        ] {
            state
                .create_open_loop_inner(
                    &OpenLoopDraft::new(
                        owner,
                        yunxi_core::OpenLoopKind::PendingQuestion,
                        format!("loop for {owner:?}"),
                    )
                    .expect("loop draft"),
                )
                .expect("create loop");
        }
        assert_eq!(
            state
                .list_open_loops_inner(OpenLoopOwner::Person(person_a), 10)
                .expect("list a")
                .len(),
            1
        );
        assert_eq!(
            state
                .list_open_loops_inner(OpenLoopOwner::Person(person_b), 10)
                .expect("list b")
                .len(),
            1
        );
    }

    #[test]
    fn snapshot_and_collection_bounds_are_enforced() {
        let path = unique_path("oversized");
        fs::write(&path, vec![b'x'; MAX_CLI_STATE_BYTES + 1]).expect("write oversized state");
        assert!(matches!(
            CliCoreState::open(&path),
            Err(CliStateError::FileTooLarge { .. })
        ));
        cleanup(&path);

        let person = PersonId::new();
        let conversation = ConversationId::new();
        let state = CliCoreState::in_memory_for(person, conversation);
        let now = Utc::now();
        for index in 0..(MAX_CLI_MEMORIES_PER_SCOPE + 5) {
            state
                .remember_message(conversation, &format!("memory {index}"), now)
                .expect("bounded remember");
        }
        assert_eq!(
            state.stats().expect("stats").memories,
            MAX_CLI_MEMORIES_PER_SCOPE
        );
        let query = MemoryQuery::new(MemoryScope::Conversation(conversation), "memory 0", 10)
            .expect("query");
        assert!(state.recall_inner(&query).expect("recall").is_empty());
        assert!(matches!(
            state.list_open_loops_inner(OpenLoopOwner::Global, 129),
            Err(CliStateError::Invalid { .. })
        ));

        let mut snapshot = StateSnapshot::new(person, conversation);
        for index in 0..=MAX_CLI_MEMORIES_PER_SCOPE {
            let draft = MemoryDraft::new(
                MemoryScope::Conversation(conversation),
                MemoryKind::Conversation,
                format!("injected memory {index}"),
                now,
            )
            .expect("memory draft");
            snapshot
                .memories
                .push(Memory::from_draft(MemoryId::new(), &draft, now).expect("injected memory"));
        }
        assert!(matches!(
            snapshot.validate(),
            Err(CliStateError::Capacity {
                kind: "memories per scope",
                ..
            })
        ));

        snapshot.memories.clear();
        for index in 0..=MAX_CLI_OPEN_LOOPS_PER_OWNER {
            let draft = OpenLoopDraft::new(
                OpenLoopOwner::Conversation(conversation),
                yunxi_core::OpenLoopKind::FollowUp,
                format!("injected loop {index}"),
            )
            .expect("loop draft");
            snapshot
                .open_loops
                .push(OpenLoop::from_draft(OpenLoopId::new(), &draft, now).expect("injected loop"));
        }
        assert!(matches!(
            snapshot.validate(),
            Err(CliStateError::Capacity {
                kind: "active open loops per owner",
                ..
            })
        ));
    }

    #[test]
    fn corrupt_snapshot_fails_closed() {
        let path = unique_path("corrupt");
        fs::write(&path, br#"{"schema_version":1"#).expect("write corrupt state");
        assert!(matches!(
            CliCoreState::open(&path),
            Err(CliStateError::Decode(_))
        ));
        cleanup(&path);
    }
}
