//! Admission and execution policy for Core actions.
//!
//! The arbiter is deliberately small and deterministic.  It validates an
//! action, checks host capabilities and authorization, rejects stale or
//! replayed decisions, reserves cooldown/rate-limit state, and only then
//! calls the platform adapter through [`ActionPort`].

use crate::action::{ActionId, ActionScope, ActionValidationError, ProposedAction};
use crate::delivery::{DeliveryResolutionError, DeliveryResolver};
use crate::identity::{ConversationId, MessageId, PersonId};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;

pub const MAX_TRACKED_ACTION_KEYS: usize = 4_096;
pub const MAX_TRACKED_ACTION_SCOPES: usize = 4_096;
pub const MAX_RATE_LIMIT_WINDOW_ENTRIES: usize = 4_096;

/// Capabilities exposed by a host adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCapability {
    SendMessage,
    ReachOut,
}

impl ActionCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SendMessage => "send_message",
            Self::ReachOut => "reach_out",
        }
    }
}

impl fmt::Display for ActionCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A host capability, optionally restricted to a set of Core scopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDescriptor {
    pub capability: ActionCapability,
    pub allowed_scopes: Option<Vec<ActionScope>>,
}

impl ActionDescriptor {
    #[must_use]
    pub const fn new(capability: ActionCapability) -> Self {
        Self {
            capability,
            allowed_scopes: None,
        }
    }

    #[must_use]
    pub fn for_scopes<I>(capability: ActionCapability, scopes: I) -> Self
    where
        I: IntoIterator<Item = ActionScope>,
    {
        Self {
            capability,
            allowed_scopes: Some(scopes.into_iter().collect()),
        }
    }

    #[must_use]
    pub fn allows(&self, scope: ActionScope) -> bool {
        self.allowed_scopes
            .as_ref()
            .is_none_or(|scopes| scopes.contains(&scope))
    }
}

/// The set of actions currently exposed by an environment.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentCapabilities {
    pub actions: Vec<ActionDescriptor>,
}

impl EnvironmentCapabilities {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            actions: Vec::new(),
        }
    }

    #[must_use]
    pub fn all() -> Self {
        Self::empty()
            .with_action(ActionDescriptor::new(ActionCapability::SendMessage))
            .with_action(ActionDescriptor::new(ActionCapability::ReachOut))
    }

    #[must_use]
    pub fn new<I>(actions: I) -> Self
    where
        I: IntoIterator<Item = ActionDescriptor>,
    {
        Self {
            actions: actions.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn with_action(mut self, descriptor: ActionDescriptor) -> Self {
        if !self.actions.iter().any(|current| current == &descriptor) {
            self.actions.push(descriptor);
        }
        self
    }

    #[must_use]
    pub fn actions(&self) -> &[ActionDescriptor] {
        &self.actions
    }

    #[must_use]
    pub fn supports(&self, capability: ActionCapability, scope: ActionScope) -> bool {
        self.actions
            .iter()
            .any(|descriptor| descriptor.capability == capability && descriptor.allows(scope))
    }
}

/// Core-side authorization constraints.  Host adapters must enforce their
/// own platform permissions again when they execute an admitted action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationPolicy {
    allow_send_message: bool,
    allow_reach_out: bool,
    allowed_actors: Option<HashSet<PersonId>>,
    allowed_people: Option<HashSet<PersonId>>,
    allowed_conversations: Option<HashSet<ConversationId>>,
    owner: Option<PersonId>,
    admin_override: bool,
}

impl AuthorizationPolicy {
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            allow_send_message: true,
            allow_reach_out: true,
            allowed_actors: None,
            allowed_people: None,
            allowed_conversations: None,
            owner: None,
            admin_override: false,
        }
    }

    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            allow_send_message: false,
            allow_reach_out: false,
            ..Self::allow_all()
        }
    }

    #[must_use]
    pub fn allow_send_message(mut self, allowed: bool) -> Self {
        self.allow_send_message = allowed;
        self
    }

    #[must_use]
    pub fn allow_reach_out(mut self, allowed: bool) -> Self {
        self.allow_reach_out = allowed;
        self
    }

    #[must_use]
    pub fn with_allowed_actors<I>(mut self, actors: I) -> Self
    where
        I: IntoIterator<Item = PersonId>,
    {
        self.allowed_actors = Some(actors.into_iter().collect());
        self
    }

    #[must_use]
    pub fn with_allowed_people<I>(mut self, people: I) -> Self
    where
        I: IntoIterator<Item = PersonId>,
    {
        self.allowed_people = Some(people.into_iter().collect());
        self
    }

    #[must_use]
    pub fn with_allowed_conversations<I>(mut self, conversations: I) -> Self
    where
        I: IntoIterator<Item = ConversationId>,
    {
        self.allowed_conversations = Some(conversations.into_iter().collect());
        self
    }

    #[must_use]
    pub const fn with_owner(mut self, owner: Option<PersonId>) -> Self {
        self.owner = owner;
        self
    }

    #[must_use]
    pub const fn with_admin_override(mut self, enabled: bool) -> Self {
        self.admin_override = enabled;
        self
    }

    fn permits(&self, action: &ProposedAction) -> Result<(), AuthorizationFailure> {
        let actor = action.actor();
        if !self.admin_override {
            if let Some(actors) = &self.allowed_actors
                && actor.is_none_or(|actor| !actors.contains(&actor))
            {
                return Err(AuthorizationFailure::ActorNotAllowed { actor });
            }
            if let Some(owner) = self.owner
                && actor != Some(owner)
            {
                return Err(AuthorizationFailure::OwnerRequired { owner, actor });
            }
        }

        match action {
            ProposedAction::SendMessage(action) => {
                if !self.allow_send_message {
                    return Err(AuthorizationFailure::ActionNotAllowed);
                }
                if let Some(conversations) = &self.allowed_conversations
                    && !conversations.contains(&action.conversation_id)
                {
                    return Err(AuthorizationFailure::ScopeNotAllowed);
                }
            }
            ProposedAction::ReachOut(action) => {
                if !self.allow_reach_out {
                    return Err(AuthorizationFailure::ActionNotAllowed);
                }
                if let Some(people) = &self.allowed_people
                    && !people.contains(&action.person_id)
                {
                    return Err(AuthorizationFailure::ScopeNotAllowed);
                }
            }
            ProposedAction::Noop => {}
        }
        Ok(())
    }
}

impl Default for AuthorizationPolicy {
    fn default() -> Self {
        Self::allow_all()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub max_actions: u32,
    pub window: Duration,
}

impl RateLimit {
    #[must_use]
    pub const fn new(max_actions: u32, window: Duration) -> Self {
        Self {
            max_actions,
            window,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionArbiterConfig {
    pub capabilities: EnvironmentCapabilities,
    pub authorization: AuthorizationPolicy,
    pub cooldown: Duration,
    pub daily_limit: Option<u32>,
    pub rate_limit: Option<RateLimit>,
    pub generation: u64,
    pub max_action_age: Option<Duration>,
    pub max_clock_skew: Duration,
}

impl Default for ActionArbiterConfig {
    fn default() -> Self {
        Self {
            // Hosts must explicitly publish the operations they support. A
            // missing capability declaration must fail closed.
            capabilities: EnvironmentCapabilities::empty(),
            authorization: AuthorizationPolicy::allow_all(),
            cooldown: Duration::ZERO,
            daily_limit: None,
            rate_limit: None,
            generation: 0,
            max_action_age: None,
            max_clock_skew: Duration::from_secs(30),
        }
    }
}

impl ActionArbiterConfig {
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: EnvironmentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub fn with_authorization(mut self, authorization: AuthorizationPolicy) -> Self {
        self.authorization = authorization;
        self
    }

    #[must_use]
    pub const fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    #[must_use]
    pub const fn with_daily_limit(mut self, daily_limit: Option<u32>) -> Self {
        self.daily_limit = daily_limit;
        self
    }

    #[must_use]
    pub const fn with_rate_limit(mut self, rate_limit: Option<RateLimit>) -> Self {
        self.rate_limit = rate_limit;
        self
    }

    #[must_use]
    pub const fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    #[must_use]
    pub const fn with_max_action_age(mut self, max_action_age: Option<Duration>) -> Self {
        self.max_action_age = max_action_age;
        self
    }

    #[must_use]
    pub const fn with_max_clock_skew(mut self, max_clock_skew: Duration) -> Self {
        self.max_clock_skew = max_clock_skew;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorizationFailure {
    ActionNotAllowed,
    ActorNotAllowed {
        actor: Option<PersonId>,
    },
    OwnerRequired {
        owner: PersonId,
        actor: Option<PersonId>,
    },
    ScopeNotAllowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    Expired {
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
    FutureIssued {
        issued_at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
    GenerationMismatch {
        expected: u64,
        actual: u64,
    },
    TooOld {
        issued_at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ActionRejection {
    #[error("action validation failed: {error}")]
    Invalid {
        action_id: Option<ActionId>,
        #[source]
        error: ActionValidationError,
    },
    #[error("action capability {capability} is unavailable")]
    CapabilityUnavailable {
        action_id: Option<ActionId>,
        capability: ActionCapability,
    },
    #[error("action is unauthorized: {reason}")]
    Unauthorized {
        action_id: Option<ActionId>,
        reason: String,
    },
    #[error("action scope is not allowed")]
    ScopeNotAllowed {
        action_id: Option<ActionId>,
        scope: ActionScope,
    },
    #[error("action is stale: {reason:?}")]
    Stale {
        action_id: Option<ActionId>,
        reason: StaleReason,
    },
    #[error("action cooldown is active until {retry_at}")]
    CooldownActive {
        action_id: Option<ActionId>,
        scope: ActionScope,
        retry_at: DateTime<Utc>,
    },
    #[error("action rate limit is active until {retry_at}")]
    RateLimitExceeded {
        action_id: Option<ActionId>,
        retry_at: DateTime<Utc>,
    },
    #[error("daily action limit of {limit} has been reached")]
    DailyLimitExceeded {
        action_id: Option<ActionId>,
        limit: u32,
    },
    #[error("action idempotency key `{idempotency_key}` was already admitted")]
    Duplicate {
        action_id: Option<ActionId>,
        idempotency_key: String,
        original_action_id: ActionId,
    },
    #[error("action idempotency state is full")]
    IdempotencyStateFull { action_id: Option<ActionId> },
    #[error("no delivery route is available for person {person_id}")]
    TargetUnavailable {
        action_id: Option<ActionId>,
        person_id: PersonId,
    },
    #[error("delivery resolution failed: {error}")]
    DeliveryResolutionFailed {
        action_id: Option<ActionId>,
        error: String,
    },
}

impl ActionRejection {
    #[must_use]
    pub const fn action_id(&self) -> Option<ActionId> {
        match self {
            Self::Invalid { action_id, .. }
            | Self::CapabilityUnavailable { action_id, .. }
            | Self::Unauthorized { action_id, .. }
            | Self::ScopeNotAllowed { action_id, .. }
            | Self::Stale { action_id, .. }
            | Self::CooldownActive { action_id, .. }
            | Self::RateLimitExceeded { action_id, .. }
            | Self::DailyLimitExceeded { action_id, .. }
            | Self::Duplicate { action_id, .. }
            | Self::IdempotencyStateFull { action_id }
            | Self::TargetUnavailable { action_id, .. }
            | Self::DeliveryResolutionFailed { action_id, .. } => *action_id,
        }
    }
}

/// Result returned by a host adapter after it receives an admitted action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionPortOutcome {
    Delivered {
        external_reference: Option<String>,
        message_id: Option<MessageId>,
        conversation_id: Option<ConversationId>,
    },
    Deferred {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("action port failed: {category}")]
pub struct ActionPortError {
    pub category: String,
    pub retryable: bool,
}

impl ActionPortError {
    #[must_use]
    pub fn new(category: impl Into<String>, retryable: bool) -> Self {
        Self {
            category: category.into(),
            retryable,
        }
    }
}

pub type ActionPortFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ActionPortOutcome, ActionPortError>> + Send + 'a>,
>;

/// Object-safe side-effect boundary implemented by each host adapter.
pub trait ActionPort: Send + Sync {
    fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionReceipt {
    pub action_id: Option<ActionId>,
    pub idempotency_key: Option<String>,
    pub admitted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    Noop,
    Executed {
        receipt: ActionReceipt,
        outcome: ActionPortOutcome,
    },
    Failed {
        receipt: ActionReceipt,
        error: ActionPortError,
    },
    Rejected(ActionRejection),
}

impl ActionResult {
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(
            self,
            Self::Noop
                | Self::Executed {
                    outcome: ActionPortOutcome::Delivered { .. },
                    ..
                }
        )
    }

    #[must_use]
    pub const fn rejection(&self) -> Option<&ActionRejection> {
        match self {
            Self::Rejected(rejection) => Some(rejection),
            Self::Noop | Self::Executed { .. } | Self::Failed { .. } => None,
        }
    }
}

#[derive(Debug, Default)]
struct ArbiterState {
    admitted_keys: HashMap<String, AdmittedAction>,
    admitted_key_order: VecDeque<String>,
    last_by_scope: HashMap<ActionScope, DateTime<Utc>>,
    rate_events: VecDeque<DateTime<Utc>>,
    daily_date: Option<NaiveDate>,
    daily_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdmittedAction {
    action_id: ActionId,
    delivered: bool,
}

/// Validates and dispatches proposed actions without knowing a platform API.
pub struct ActionArbiter {
    config: ActionArbiterConfig,
    state: Mutex<ArbiterState>,
    delivery_resolver: Option<Arc<dyn DeliveryResolver>>,
}

impl fmt::Debug for ActionArbiter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActionArbiter")
            .field("config", &self.config)
            .field("delivery_resolver", &self.delivery_resolver.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for ActionArbiter {
    fn default() -> Self {
        Self::new(ActionArbiterConfig::default())
    }
}

impl ActionArbiter {
    #[must_use]
    pub fn new(config: ActionArbiterConfig) -> Self {
        Self {
            config,
            state: Mutex::new(ArbiterState::default()),
            delivery_resolver: None,
        }
    }

    #[must_use]
    pub fn with_delivery_resolver(mut self, resolver: Arc<dyn DeliveryResolver>) -> Self {
        self.delivery_resolver = Some(resolver);
        self
    }

    #[must_use]
    pub const fn config(&self) -> &ActionArbiterConfig {
        &self.config
    }

    /// Performs all stateless checks.  Cooldown, rate, and idempotency state
    /// are reserved by [`Self::admit_at`].
    pub fn validate_at(
        &self,
        action: &ProposedAction,
        now: DateTime<Utc>,
    ) -> Result<(), ActionRejection> {
        action
            .validate()
            .map_err(|error| ActionRejection::Invalid {
                action_id: action.action_id(),
                error,
            })?;
        if let Some(issued_at) = action.issued_at() {
            let max_skew = chrono::Duration::from_std(self.config.max_clock_skew)
                .unwrap_or(chrono::Duration::MAX);
            if issued_at > now + max_skew {
                return Err(ActionRejection::Stale {
                    action_id: action.action_id(),
                    reason: StaleReason::FutureIssued { issued_at, now },
                });
            }
            if let Some(expires_at) = action.expires_at()
                && now >= expires_at
            {
                return Err(ActionRejection::Stale {
                    action_id: action.action_id(),
                    reason: StaleReason::Expired { expires_at, now },
                });
            }
            if let Some(max_age) = self.config.max_action_age {
                let max_age = chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::MAX);
                if now - issued_at > max_age {
                    return Err(ActionRejection::Stale {
                        action_id: action.action_id(),
                        reason: StaleReason::TooOld { issued_at, now },
                    });
                }
            }
            if let Some(generation) = action.generation()
                && generation != self.config.generation
            {
                return Err(ActionRejection::Stale {
                    action_id: action.action_id(),
                    reason: StaleReason::GenerationMismatch {
                        expected: self.config.generation,
                        actual: generation,
                    },
                });
            }
        }

        let scope = action.scope();
        if let Some(capability) = capability_for(action)
            && !self.config.capabilities.supports(capability, scope)
        {
            return Err(ActionRejection::CapabilityUnavailable {
                action_id: action.action_id(),
                capability,
            });
        }
        if let Err(failure) = self.config.authorization.permits(action) {
            let action_id = action.action_id();
            return match failure {
                AuthorizationFailure::ScopeNotAllowed => {
                    Err(ActionRejection::ScopeNotAllowed { action_id, scope })
                }
                other => Err(ActionRejection::Unauthorized {
                    action_id,
                    reason: other.to_string(),
                }),
            };
        }
        Ok(())
    }

    /// Atomically reserves idempotency, cooldown, rate, and daily-limit
    /// state.  The port is called only after this method succeeds.
    pub fn admit_at(
        &self,
        action: &ProposedAction,
        now: DateTime<Utc>,
    ) -> Result<ActionReceipt, ActionRejection> {
        self.validate_at(action, now)?;
        let Some(key) = action.idempotency_key() else {
            return Ok(ActionReceipt {
                action_id: None,
                idempotency_key: None,
                admitted_at: now,
            });
        };
        let action_id = action.action_id();
        let scope = action.scope();
        let mut state = self
            .state
            .lock()
            .expect("action arbiter state lock poisoned");

        if let Some(original) = state.admitted_keys.get(key).copied() {
            return Err(ActionRejection::Duplicate {
                action_id,
                idempotency_key: key.to_owned(),
                original_action_id: original.action_id,
            });
        }
        if self.config.cooldown > Duration::ZERO {
            if let Some(last) = state.last_by_scope.get(&scope).copied() {
                let cooldown = chrono::Duration::from_std(self.config.cooldown)
                    .unwrap_or(chrono::Duration::MAX);
                let retry_at = last + cooldown;
                if now < retry_at {
                    return Err(ActionRejection::CooldownActive {
                        action_id,
                        scope,
                        retry_at,
                    });
                }
            }
            if state.last_by_scope.len() >= MAX_TRACKED_ACTION_SCOPES
                && !state.last_by_scope.contains_key(&scope)
            {
                return Err(ActionRejection::IdempotencyStateFull { action_id });
            }
        }

        if let Some(rate_limit) = self.config.rate_limit {
            if rate_limit.max_actions == 0 || rate_limit.window.is_zero() {
                return Err(ActionRejection::RateLimitExceeded {
                    action_id,
                    retry_at: now,
                });
            }
            let window =
                chrono::Duration::from_std(rate_limit.window).unwrap_or(chrono::Duration::MAX);
            while state
                .rate_events
                .front()
                .is_some_and(|timestamp| *timestamp + window <= now)
            {
                state.rate_events.pop_front();
            }
            if state.rate_events.len() >= rate_limit.max_actions as usize {
                let retry_at = state
                    .rate_events
                    .front()
                    .copied()
                    .map(|timestamp| timestamp + window)
                    .unwrap_or(now);
                return Err(ActionRejection::RateLimitExceeded {
                    action_id,
                    retry_at,
                });
            }
            if state.rate_events.len() >= MAX_RATE_LIMIT_WINDOW_ENTRIES {
                return Err(ActionRejection::RateLimitExceeded {
                    action_id,
                    retry_at: now,
                });
            }
        }

        let today = now.date_naive();
        if state.daily_date != Some(today) {
            state.daily_date = Some(today);
            state.daily_count = 0;
        }
        if let Some(limit) = self.config.daily_limit
            && state.daily_count >= limit
        {
            return Err(ActionRejection::DailyLimitExceeded { action_id, limit });
        }

        if state.admitted_keys.len() >= MAX_TRACKED_ACTION_KEYS {
            let Some(delivered_index) = state.admitted_key_order.iter().position(|candidate| {
                state
                    .admitted_keys
                    .get(candidate)
                    .is_some_and(|admitted| admitted.delivered)
            }) else {
                return Err(ActionRejection::IdempotencyStateFull { action_id });
            };
            if let Some(oldest_delivered) = state.admitted_key_order.remove(delivered_index) {
                state.admitted_keys.remove(&oldest_delivered);
            }
        }
        let key = key.to_owned();
        state.admitted_keys.insert(
            key.clone(),
            AdmittedAction {
                action_id: action_id.unwrap_or_default(),
                delivered: false,
            },
        );
        state.admitted_key_order.push_back(key.clone());
        if self.config.cooldown > Duration::ZERO {
            state.last_by_scope.insert(scope, now);
        }
        if self.config.rate_limit.is_some() {
            state.rate_events.push_back(now);
        }
        state.daily_count = state.daily_count.saturating_add(1);
        Ok(ActionReceipt {
            action_id,
            idempotency_key: Some(key),
            admitted_at: now,
        })
    }

    fn mark_delivered(&self, receipt: &ActionReceipt) {
        let (Some(key), Some(action_id)) = (&receipt.idempotency_key, receipt.action_id) else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .expect("action arbiter state lock poisoned");
        if let Some(admitted) = state.admitted_keys.get_mut(key)
            && admitted.action_id == action_id
        {
            admitted.delivered = true;
        }
    }

    fn release_reservation(&self, receipt: &ActionReceipt) {
        let (Some(key), Some(action_id)) = (&receipt.idempotency_key, receipt.action_id) else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .expect("action arbiter state lock poisoned");
        if state
            .admitted_keys
            .get(key)
            .is_some_and(|admitted| admitted.action_id == action_id)
        {
            state.admitted_keys.remove(key);
            state
                .admitted_key_order
                .retain(|candidate| candidate != key);
        }
    }

    pub(crate) fn was_delivered(
        &self,
        idempotency_key: &str,
        original_action_id: ActionId,
    ) -> bool {
        self.state
            .lock()
            .expect("action arbiter state lock poisoned")
            .admitted_keys
            .get(idempotency_key)
            .is_some_and(|admitted| admitted.action_id == original_action_id && admitted.delivered)
    }

    /// Dispatches using the current wall clock.
    pub async fn dispatch(&self, action: ProposedAction, port: &dyn ActionPort) -> ActionResult {
        self.dispatch_at(action, port, Utc::now()).await
    }

    /// Deterministic dispatch entry point used by tests and schedulers.
    pub async fn dispatch_at(
        &self,
        action: ProposedAction,
        port: &dyn ActionPort,
        now: DateTime<Utc>,
    ) -> ActionResult {
        // Reject malformed, stale, unauthorized, or unsupported actions before
        // consulting a host resolver. This keeps validation and authorization
        // independent from target availability and avoids leaking resolver
        // information to callers that could not execute the action anyway.
        if let Err(rejection) = self.validate_at(&action, now) {
            return ActionResult::Rejected(rejection);
        }
        if let ProposedAction::ReachOut(reach_out) = &action
            && let Some(resolver) = &self.delivery_resolver
        {
            match resolver.resolve(reach_out.person_id).await {
                Ok(_route) => {}
                Err(DeliveryResolutionError::Unavailable { person_id }) => {
                    return ActionResult::Rejected(ActionRejection::TargetUnavailable {
                        action_id: Some(reach_out.action_id()),
                        person_id,
                    });
                }
                Err(error) => {
                    return ActionResult::Rejected(ActionRejection::DeliveryResolutionFailed {
                        action_id: Some(reach_out.action_id()),
                        error: error.to_string(),
                    });
                }
            }
        }

        if matches!(action, ProposedAction::Noop) {
            return ActionResult::Noop;
        }
        let receipt = match self.admit_at(&action, now) {
            Ok(receipt) => receipt,
            Err(rejection) => return ActionResult::Rejected(rejection),
        };
        match port.execute(&action).await {
            Ok(outcome @ ActionPortOutcome::Delivered { .. }) => {
                self.mark_delivered(&receipt);
                ActionResult::Executed { receipt, outcome }
            }
            Ok(outcome @ ActionPortOutcome::Deferred { .. }) => {
                self.release_reservation(&receipt);
                ActionResult::Executed { receipt, outcome }
            }
            Err(error) => {
                self.release_reservation(&receipt);
                ActionResult::Failed { receipt, error }
            }
        }
    }
}

fn capability_for(action: &ProposedAction) -> Option<ActionCapability> {
    match action {
        ProposedAction::SendMessage(_) => Some(ActionCapability::SendMessage),
        ProposedAction::ReachOut(_) => Some(ActionCapability::ReachOut),
        ProposedAction::Noop => None,
    }
}

impl fmt::Display for AuthorizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActionNotAllowed => formatter.write_str("action type is not allowed"),
            Self::ActorNotAllowed { actor } => write!(formatter, "actor {actor:?} is not allowed"),
            Self::OwnerRequired { owner, actor } => {
                write!(formatter, "owner {owner} is required, actor is {actor:?}")
            }
            Self::ScopeNotAllowed => formatter.write_str("target scope is not allowed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{ActionMetadata, ReachOutAction, SendMessageAction};
    use crate::delivery::{DeliveryResolverFuture, DeliveryRoute};
    use crate::proactive::ProactiveMotive;
    use crate::{ConversationKind, MessageContent};
    use chrono::Duration as ChronoDuration;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakePort {
        calls: AtomicUsize,
    }

    impl ActionPort for FakePort {
        fn execute<'a>(&'a self, _action: &'a ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ActionPortOutcome::Delivered {
                    external_reference: None,
                    message_id: None,
                    conversation_id: None,
                })
            })
        }
    }

    fn send_with_key(
        conversation: ConversationId,
        key: &str,
        issued_at: DateTime<Utc>,
    ) -> ProposedAction {
        let metadata =
            ActionMetadata::with_idempotency_key(key, issued_at).expect("valid metadata");
        ProposedAction::SendMessage(
            SendMessageAction::with_metadata(
                conversation,
                MessageContent::text("hello"),
                None,
                metadata,
            )
            .expect("valid action"),
        )
    }

    #[test]
    fn capability_and_authorization_are_checked_before_admission() {
        let conversation = ConversationId::new();
        let action = send_with_key(conversation, "capability", Utc::now());
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::empty()),
        );
        assert!(matches!(
            arbiter.admit_at(&action, Utc::now()),
            Err(ActionRejection::CapabilityUnavailable {
                capability: ActionCapability::SendMessage,
                ..
            })
        ));

        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default()
                .with_capabilities(EnvironmentCapabilities::all())
                .with_authorization(AuthorizationPolicy::deny_all().allow_send_message(false)),
        );
        assert!(matches!(
            arbiter.admit_at(&action, Utc::now()),
            Err(ActionRejection::Unauthorized { .. })
        ));
    }

    #[test]
    fn stale_generation_and_expiry_are_rejected() {
        let now = Utc::now();
        let metadata =
            ActionMetadata::with_idempotency_key("stale", now - ChronoDuration::seconds(5))
                .expect("metadata")
                .with_generation(7)
                .with_expiry(Some(now - ChronoDuration::seconds(1)));
        let action = ProposedAction::SendMessage(
            SendMessageAction::with_metadata(
                ConversationId::new(),
                MessageContent::text("hello"),
                None,
                metadata,
            )
            .expect("action should be structurally valid"),
        );
        let arbiter = ActionArbiter::new(ActionArbiterConfig::default().with_generation(7));
        assert!(matches!(
            arbiter.validate_at(&action, now),
            Err(ActionRejection::Stale {
                reason: StaleReason::Expired { .. },
                ..
            })
        ));

        let action = send_with_key(ConversationId::new(), "generation", now);
        let metadata = match action {
            ProposedAction::SendMessage(action) => action.metadata.with_generation(3),
            ProposedAction::ReachOut(_) | ProposedAction::Noop => unreachable!(),
        };
        let action = ProposedAction::SendMessage(
            SendMessageAction::with_metadata(
                ConversationId::new(),
                MessageContent::text("hello"),
                None,
                metadata,
            )
            .expect("action"),
        );
        let arbiter = ActionArbiter::new(ActionArbiterConfig::default().with_generation(2));
        assert!(matches!(
            arbiter.validate_at(&action, now),
            Err(ActionRejection::Stale {
                reason: StaleReason::GenerationMismatch { .. },
                ..
            })
        ));
    }

    #[test]
    fn cooldown_rate_daily_and_idempotency_are_bounded() {
        let now = Utc::now();
        let conversation = ConversationId::new();
        let config = ActionArbiterConfig::default()
            .with_cooldown(Duration::from_secs(30))
            .with_rate_limit(Some(RateLimit::new(2, Duration::from_secs(60))))
            .with_daily_limit(Some(3));
        let arbiter = ActionArbiter::new(config.with_capabilities(EnvironmentCapabilities::all()));
        let first = send_with_key(conversation, "first", now);
        arbiter.admit_at(&first, now).expect("first admission");
        assert!(matches!(
            arbiter.admit_at(&send_with_key(conversation, "duplicate", now), now),
            Err(ActionRejection::CooldownActive { .. })
        ));

        let later = now + ChronoDuration::seconds(31);
        let second = send_with_key(conversation, "second", later);
        arbiter.admit_at(&second, later).expect("second admission");
        assert!(matches!(
            arbiter.admit_at(&send_with_key(conversation, "second", later), later),
            Err(ActionRejection::Duplicate { .. })
        ));
        assert!(matches!(
            arbiter.admit_at(&send_with_key(ConversationId::new(), "third", later), later),
            Err(ActionRejection::RateLimitExceeded { .. })
        ));
    }

    #[test]
    fn daily_limit_rejects_after_the_configured_count() {
        let now = Utc::now();
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default()
                .with_capabilities(EnvironmentCapabilities::all())
                .with_daily_limit(Some(1)),
        );
        arbiter
            .admit_at(
                &send_with_key(ConversationId::new(), "daily-first", now),
                now,
            )
            .expect("first action should fit the daily budget");
        assert!(matches!(
            arbiter.admit_at(
                &send_with_key(ConversationId::new(), "daily-second", now),
                now,
            ),
            Err(ActionRejection::DailyLimitExceeded { limit: 1, .. })
        ));
    }

    #[test]
    fn idempotency_history_evicts_the_oldest_key_at_capacity() {
        let now = Utc::now();
        let conversation = ConversationId::new();
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let mut oldest_receipt = None;
        for index in 0..MAX_TRACKED_ACTION_KEYS {
            let receipt = arbiter
                .admit_at(
                    &send_with_key(conversation, &format!("bounded-{index}"), now),
                    now,
                )
                .expect("history entry should fit");
            if index == 0 {
                oldest_receipt = Some(receipt.clone());
            }
            arbiter.mark_delivered(&receipt);
        }

        arbiter
            .admit_at(&send_with_key(conversation, "newest", now), now)
            .expect("a full history should evict its oldest entry");
        assert!(matches!(
            arbiter.admit_at(&send_with_key(conversation, "newest", now), now),
            Err(ActionRejection::Duplicate { .. })
        ));
        let replacement = arbiter
            .admit_at(&send_with_key(conversation, "bounded-0", now), now)
            .expect("the oldest idempotency key should have been evicted");
        arbiter.release_reservation(
            &oldest_receipt.expect("the oldest admission receipt should be recorded"),
        );
        assert!(matches!(
            arbiter.admit_at(&send_with_key(conversation, "bounded-0", now), now),
            Err(ActionRejection::Duplicate {
                original_action_id,
                ..
            }) if Some(original_action_id) == replacement.action_id
        ));
    }

    #[test]
    fn idempotency_history_never_evicts_in_flight_reservations() {
        let now = Utc::now();
        let conversation = ConversationId::new();
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        for index in 0..MAX_TRACKED_ACTION_KEYS {
            arbiter
                .admit_at(
                    &send_with_key(conversation, &format!("in-flight-{index}"), now),
                    now,
                )
                .expect("in-flight reservation should fit");
        }

        assert!(matches!(
            arbiter.admit_at(&send_with_key(conversation, "overflow", now), now),
            Err(ActionRejection::IdempotencyStateFull { .. })
        ));
        assert!(matches!(
            arbiter.admit_at(&send_with_key(conversation, "in-flight-0", now), now),
            Err(ActionRejection::Duplicate { .. })
        ));
    }

    struct DeferredPort;

    impl ActionPort for DeferredPort {
        fn execute<'a>(&'a self, _action: &'a ProposedAction) -> ActionPortFuture<'a> {
            Box::pin(async {
                Ok(ActionPortOutcome::Deferred {
                    reason: "temporarily offline".to_owned(),
                })
            })
        }
    }

    #[tokio::test]
    async fn deferred_delivery_releases_its_idempotency_reservation() {
        let now = Utc::now();
        let action = send_with_key(ConversationId::new(), "retry-deferred", now);
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        assert!(matches!(
            arbiter
                .dispatch_at(action.clone(), &DeferredPort, now)
                .await,
            ActionResult::Executed {
                outcome: ActionPortOutcome::Deferred { .. },
                ..
            }
        ));
        let delivered = FakePort {
            calls: AtomicUsize::new(0),
        };
        assert!(matches!(
            arbiter.dispatch_at(action, &delivered, now).await,
            ActionResult::Executed {
                outcome: ActionPortOutcome::Delivered { .. },
                ..
            }
        ));
        assert_eq!(delivered.calls.load(Ordering::SeqCst), 1);
    }

    struct UnavailableResolver;

    impl DeliveryResolver for UnavailableResolver {
        fn resolve<'a>(&'a self, person_id: PersonId) -> DeliveryResolverFuture<'a> {
            Box::pin(async move { Err(DeliveryResolutionError::Unavailable { person_id }) })
        }
    }

    #[tokio::test]
    async fn dispatch_resolves_target_before_calling_port_and_dedupes() {
        let person = PersonId::new();
        let action = ProposedAction::ReachOut(
            ReachOutAction::new(
                person,
                MessageContent::text("hello"),
                ProactiveMotive::CheckIn,
            )
            .expect("action"),
        );
        let port = FakePort {
            calls: AtomicUsize::new(0),
        };
        let unavailable = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        )
        .with_delivery_resolver(Arc::new(UnavailableResolver));
        assert!(matches!(
            unavailable.dispatch(action.clone(), &port).await,
            ActionResult::Rejected(ActionRejection::TargetUnavailable { .. })
        ));
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);

        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let first = arbiter.dispatch(action.clone(), &port).await;
        assert!(matches!(first, ActionResult::Executed { .. }));
        let second = arbiter.dispatch(action, &port).await;
        assert!(matches!(
            second,
            ActionResult::Rejected(ActionRejection::Duplicate { .. })
        ));
        assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn deferred_delivery_is_not_reported_as_success() {
        let receipt = ActionReceipt {
            action_id: Some(ActionId::new()),
            idempotency_key: Some("deferred".to_owned()),
            admitted_at: Utc::now(),
        };
        let result = ActionResult::Executed {
            receipt,
            outcome: ActionPortOutcome::Deferred {
                reason: "offline".to_owned(),
            },
        };
        assert!(!result.is_success());
    }

    #[test]
    fn zero_length_rate_windows_fail_closed() {
        let now = Utc::now();
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default()
                .with_capabilities(EnvironmentCapabilities::all())
                .with_rate_limit(Some(RateLimit::new(1, Duration::ZERO))),
        );
        assert!(matches!(
            arbiter.admit_at(
                &send_with_key(ConversationId::new(), "zero-window", now),
                now,
            ),
            Err(ActionRejection::RateLimitExceeded { .. })
        ));
    }

    #[test]
    fn scoped_capabilities_reject_cross_scope_actions() {
        let allowed = ConversationId::new();
        let denied = ConversationId::new();
        let capabilities = EnvironmentCapabilities::new([ActionDescriptor::for_scopes(
            ActionCapability::SendMessage,
            [ActionScope::Conversation(allowed)],
        )]);
        let arbiter =
            ActionArbiter::new(ActionArbiterConfig::default().with_capabilities(capabilities));
        let action = send_with_key(denied, "scope", Utc::now());
        assert!(matches!(
            arbiter.validate_at(&action, Utc::now()),
            Err(ActionRejection::CapabilityUnavailable { .. })
        ));
    }

    #[allow(dead_code)]
    fn _route_is_platform_neutral() -> DeliveryRoute {
        DeliveryRoute::new(ConversationId::new(), ConversationKind::Direct)
    }
}
