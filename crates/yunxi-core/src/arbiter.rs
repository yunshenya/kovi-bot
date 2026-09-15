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
use std::time::{Duration, Instant};
use thiserror::Error;

pub const MAX_TRACKED_ACTION_KEYS: usize = 4_096;
pub const MAX_TRACKED_ACTION_SCOPES: usize = 4_096;
pub const MAX_RATE_LIMIT_WINDOW_ENTRIES: usize = 4_096;

/// Acquire the shared arbiter state lock, recovering the inner value if a
/// previous holder panicked. The arbiter is shared across every conversation,
/// so a single poisoned lock currently cascades into a panic on every later
/// admission and takes the whole loop down. Recovering the guard keeps
/// following admissions fail-closed instead of crashing; the recovered state is
/// still internally consistent because arbitration never holds the lock across
/// an `await` and only panics on genuinely impossible branch conditions.
fn lock_arbiter_state(state: &Mutex<ArbiterState>) -> std::sync::MutexGuard<'_, ArbiterState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Capabilities exposed by a host adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCapability {
    SendMessage,
    ReachOut,
    /// Open a live voice call with a person.
    ///
    /// Deliberately separate from [`Self::ReachOut`]: reaching out is a message
    /// a person can ignore, a call is one they must answer or reject. Hosts that
    /// can do the second may not be able to do the first at all (the QQ call
    /// channel is a separate bridge, not the message transport), so the two are
    /// declared and withheld independently.
    StartCall,
    UseTool,
    CreateOpenLoop,
    ResolveOpenLoop,
    StartGoal,
    CancelGoal,
}

impl ActionCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SendMessage => "send_message",
            Self::ReachOut => "reach_out",
            Self::StartCall => "start_call",
            Self::UseTool => "use_tool",
            Self::CreateOpenLoop => "create_open_loop",
            Self::ResolveOpenLoop => "resolve_open_loop",
            Self::StartGoal => "start_goal",
            Self::CancelGoal => "cancel_goal",
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
    /// Which tool this declares, for hosts that expose tools.
    ///
    /// A `UseTool` intent names a tool, not a capability, so Core cannot match a
    /// call to its declaration without this: the capability says "this host can
    /// use tools", never "this particular call only reads".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// How far this tool's effects reach.
    ///
    /// Declared by the host, because only the host knows what a call does.
    /// Defaults to [`EffectScope::Outbound`] so an undeclared or unknown action
    /// fails closed rather than being treated as harmless.
    #[serde(default)]
    pub effect: EffectScope,
    /// Whether this tool's result can contain text written by someone else.
    ///
    /// A web page, a search result, a remote server, another person's nickname:
    /// after one of those enters a task, speaking in her name on the strength of
    /// it is not something she should do. This is a host fact about a tool, so
    /// it is declared here rather than re-derived by Core — and it defaults to
    /// true, so a forgotten declaration cannot quietly earn a wider ceiling.
    #[serde(default = "default_may_carry_foreign_text")]
    pub may_carry_foreign_text: bool,
}

const fn default_may_carry_foreign_text() -> bool {
    true
}

/// How far an action's effects reach.
///
/// See [`EffectScope`] variants for what each tier admits.
///
/// This is the declaration Core needs to decide whether a call is allowed given
/// what the current turn has been exposed to. It is deliberately about *reach*,
/// not about how dangerous the call looks: a web search and a memory write are
/// both "safe" and belong in different tiers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectScope {
    /// No side effect at all: a query.
    ReadOnly,
    /// Changes only the acting person's own state, reversibly and without
    /// becoming visible to anyone else: a reminder, a private memory.
    UserScoped,
    /// Speaks as the agent, or changes state shared with others.
    ///
    /// The default, so a declaration that was forgotten cannot silently pass
    /// for harmless.
    #[default]
    Outbound,
}

impl ActionDescriptor {
    #[must_use]
    pub const fn new(capability: ActionCapability) -> Self {
        Self {
            capability,
            allowed_scopes: None,
            tool: None,
            effect: EffectScope::Outbound,
            may_carry_foreign_text: false,
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
            tool: None,
            effect: EffectScope::Outbound,
            may_carry_foreign_text: false,
        }
    }

    /// Declares one tool this host exposes, how far its effects reach, and
    /// whether its result can carry text written by someone else.
    #[must_use]
    pub fn tool(
        name: impl Into<String>,
        effect: EffectScope,
        may_carry_foreign_text: bool,
    ) -> Self {
        Self {
            capability: ActionCapability::UseTool,
            allowed_scopes: None,
            tool: Some(name.into()),
            effect,
            may_carry_foreign_text,
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

    /// Every **platform-neutral** action capability.
    ///
    /// [`ActionCapability::StartCall`] is deliberately absent. Placing a call
    /// needs an out-of-band voice channel that no host gets for free — the QQ
    /// host drives a separate NapCat AV bridge with its own audio devices — so a
    /// host that can talk cannot necessarily ring. Hosts that can must declare
    /// it explicitly (see the QQ adapter's `capabilities()`), and a host that
    /// gets it from here by accident would be advertising a channel it cannot
    /// actually open.
    #[must_use]
    pub fn all() -> Self {
        Self::empty()
            .with_action(ActionDescriptor::new(ActionCapability::SendMessage))
            .with_action(ActionDescriptor::new(ActionCapability::ReachOut))
            .with_action(ActionDescriptor::new(ActionCapability::UseTool))
            .with_action(ActionDescriptor::new(ActionCapability::CreateOpenLoop))
            .with_action(ActionDescriptor::new(ActionCapability::ResolveOpenLoop))
            .with_action(ActionDescriptor::new(ActionCapability::StartGoal))
            .with_action(ActionDescriptor::new(ActionCapability::CancelGoal))
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

    /// Declares one tool this environment can call, and how far it reaches.
    ///
    /// A capability entry for `UseTool` says only that tools exist. A `UseTool`
    /// intent names a *tool*, so without per-tool declarations Core has nothing
    /// to check a call against — and an undeclared tool is indistinguishable
    /// from a hallucinated one.
    #[must_use]
    pub fn with_tool(
        self,
        name: impl Into<String>,
        effect: EffectScope,
        may_carry_foreign_text: bool,
    ) -> Self {
        self.with_action(ActionDescriptor::tool(name, effect, may_carry_foreign_text))
    }

    /// How far the named tool's effects reach, when the host declared it.
    ///
    /// Returns `None` for a tool nobody declared, which callers must treat as
    /// "not permitted" rather than "harmless".
    #[must_use]
    pub fn effect_of(&self, tool_name: &str) -> Option<EffectScope> {
        self.declaration_of(tool_name)
            .map(|descriptor| descriptor.effect)
    }

    /// Whether the named tool's result can contain text written by someone
    /// else. `None` for an undeclared tool, which callers treat as unsafe.
    #[must_use]
    pub fn may_carry_foreign_text(&self, tool_name: &str) -> Option<bool> {
        self.declaration_of(tool_name)
            .map(|descriptor| descriptor.may_carry_foreign_text)
    }

    fn declaration_of(&self, tool_name: &str) -> Option<&ActionDescriptor> {
        self.actions
            .iter()
            .find(|descriptor| descriptor.tool.as_deref() == Some(tool_name))
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
    allow_use_tool: bool,
    allow_create_open_loop: bool,
    allow_resolve_open_loop: bool,
    allow_start_goal: bool,
    allow_cancel_goal: bool,
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
            allow_use_tool: true,
            allow_create_open_loop: true,
            allow_resolve_open_loop: true,
            allow_start_goal: true,
            allow_cancel_goal: true,
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
            allow_use_tool: false,
            allow_create_open_loop: false,
            allow_resolve_open_loop: false,
            allow_start_goal: false,
            allow_cancel_goal: false,
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
    pub const fn allow_use_tool(mut self, allowed: bool) -> Self {
        self.allow_use_tool = allowed;
        self
    }

    #[must_use]
    pub const fn allow_create_open_loop(mut self, allowed: bool) -> Self {
        self.allow_create_open_loop = allowed;
        self
    }

    #[must_use]
    pub const fn allow_resolve_open_loop(mut self, allowed: bool) -> Self {
        self.allow_resolve_open_loop = allowed;
        self
    }

    #[must_use]
    pub const fn allow_start_goal(mut self, allowed: bool) -> Self {
        self.allow_start_goal = allowed;
        self
    }

    #[must_use]
    pub const fn allow_cancel_goal(mut self, allowed: bool) -> Self {
        self.allow_cancel_goal = allowed;
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
            ProposedAction::UseTool(_) => {
                if !self.allow_use_tool {
                    return Err(AuthorizationFailure::ActionNotAllowed);
                }
            }
            ProposedAction::CreateOpenLoop(action) => {
                if !self.allow_create_open_loop {
                    return Err(AuthorizationFailure::ActionNotAllowed);
                }
                if let Some(people) = &self.allowed_people
                    && let crate::open_loop::OpenLoopOwner::Person(person_id) = action.draft.owner()
                    && !people.contains(&person_id)
                {
                    return Err(AuthorizationFailure::ScopeNotAllowed);
                }
                if let Some(conversations) = &self.allowed_conversations
                    && let crate::open_loop::OpenLoopOwner::Conversation(conversation_id) =
                        action.draft.owner()
                    && !conversations.contains(&conversation_id)
                {
                    return Err(AuthorizationFailure::ScopeNotAllowed);
                }
            }
            ProposedAction::ResolveOpenLoop(_action) => {
                if !self.allow_resolve_open_loop {
                    return Err(AuthorizationFailure::ActionNotAllowed);
                }
            }
            ProposedAction::StartGoal(_action) => {
                if !self.allow_start_goal {
                    return Err(AuthorizationFailure::ActionNotAllowed);
                }
            }
            ProposedAction::CancelGoal(_) => {
                if !self.allow_cancel_goal {
                    return Err(AuthorizationFailure::ActionNotAllowed);
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
    /// The largest effect any action may have on this turn.
    ///
    /// A turn that has taken in text written by someone else may still read and
    /// may still change the acting person's own state, but it must not speak in
    /// her name or change shared state. Hosts that track that trust level set
    /// this per turn; the default permits everything, which is the behaviour
    /// every existing host already has.
    pub effect_ceiling: EffectScope,
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
            effect_ceiling: EffectScope::Outbound,
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
    #[error("too many action scopes are cooling down right now")]
    CooldownStateFull { action_id: Option<ActionId> },
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
            | Self::CooldownStateFull { action_id }
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
    /// The adapter crossed an irreversible delivery boundary but cannot prove
    /// whether the platform accepted the side effect. This is terminal for
    /// automatic replay, but it is not successful delivery.
    DeliveryIndeterminate {
        reason: String,
        conversation_id: Option<ConversationId>,
    },
    ToolCompleted {
        operation: String,
        output: String,
    },
    ToolFailed {
        operation: String,
        error_category: String,
        detail: String,
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

/// Future returned when a host releases a capability that was materialized by
/// a plan but never reached the action port's execution boundary.
pub type ActionPortReleaseFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

/// Object-safe side-effect boundary implemented by each host adapter.
pub trait ActionPort: Send + Sync {
    fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a>;

    /// Release host-side reservations for an action that was never executed.
    ///
    /// The default is a no-op so existing adapters remain source-compatible.
    /// Implementations must keep this operation idempotent and must not cross
    /// an external side-effect boundary.
    fn release_unexecuted<'a>(
        &'a self,
        _action: &'a ProposedAction,
    ) -> ActionPortReleaseFuture<'a> {
        Box::pin(async {})
    }
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
                | Self::Executed {
                    outcome: ActionPortOutcome::ToolCompleted { .. },
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
    terminal: Option<AdmittedTerminal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmittedTerminal {
    Succeeded,
    Indeterminate,
    Failed,
}

/// Keeps an idempotency reservation conservative when an action future is
/// cancelled after admission. The adapter may have crossed its side-effect
/// boundary before cancellation, so releasing the reservation would permit a
/// duplicate delivery. Marking it indeterminate makes subsequent replays fail
/// closed while still allowing terminal entries to be evicted normally.
struct DispatchReservationGuard<'a> {
    arbiter: &'a ActionArbiter,
    receipt: Option<ActionReceipt>,
}

impl<'a> DispatchReservationGuard<'a> {
    fn new(arbiter: &'a ActionArbiter, receipt: &ActionReceipt) -> Self {
        Self {
            arbiter,
            receipt: Some(receipt.clone()),
        }
    }

    fn disarm(&mut self) {
        self.receipt = None;
    }
}

impl Drop for DispatchReservationGuard<'_> {
    fn drop(&mut self) {
        if let Some(receipt) = self.receipt.take() {
            self.arbiter
                .mark_terminal(&receipt, AdmittedTerminal::Indeterminate);
        }
    }
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
        // A `UseTool` capability only says that this host exposes tools; the
        // intent names a specific tool, and Core cannot judge a call it has no
        // declaration for. An undeclared tool therefore fails closed, which
        // also means a hallucinated or injected tool name cannot reach the
        // environment on the strength of the capability alone.
        if let crate::ProposedAction::UseTool(tool) = action {
            let Some(effect) = self.config.capabilities.effect_of(&tool.tool_name) else {
                return Err(ActionRejection::Unauthorized {
                    action_id: action.action_id(),
                    reason: format!("tool `{}` is not declared by this host", tool.tool_name),
                });
            };
            if effect > self.config.effect_ceiling {
                return Err(ActionRejection::Unauthorized {
                    action_id: action.action_id(),
                    reason: format!(
                        "tool `{}` reaches {:?}, beyond this turn's {:?} ceiling",
                        tool.tool_name, effect, self.config.effect_ceiling
                    ),
                });
            }
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
        let mut state = lock_arbiter_state(&self.state);

        if let Some(original) = state.admitted_keys.get(key).copied() {
            return Err(ActionRejection::Duplicate {
                action_id,
                idempotency_key: key.to_owned(),
                original_action_id: original.action_id,
            });
        }
        if self.config.cooldown > Duration::ZERO {
            let cooldown =
                chrono::Duration::from_std(self.config.cooldown).unwrap_or(chrono::Duration::MAX);
            if let Some(last) = state.last_by_scope.get(&scope).copied() {
                let retry_at = last + cooldown;
                if now < retry_at {
                    return Err(ActionRejection::CooldownActive {
                        action_id,
                        scope,
                        retry_at,
                    });
                }
            }
            // 冷却已经过去的条目没有任何用（查一次只会得出"不拦"），却一直占着
            // 名额。不清理的话这张表只增不减：4096 个会话之后每个**新**会话都会被
            // 拒绝——而且报的是"幂等状态已满"，条件和原因都不对。
            if state.last_by_scope.len() >= MAX_TRACKED_ACTION_SCOPES {
                state.last_by_scope.retain(|_, last| now < *last + cooldown);
            }
            if state.last_by_scope.len() >= MAX_TRACKED_ACTION_SCOPES
                && !state.last_by_scope.contains_key(&scope)
            {
                return Err(ActionRejection::CooldownStateFull { action_id });
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
            let Some(terminal_index) = state.admitted_key_order.iter().position(|candidate| {
                state
                    .admitted_keys
                    .get(candidate)
                    .is_some_and(|admitted| admitted.terminal.is_some())
            }) else {
                return Err(ActionRejection::IdempotencyStateFull { action_id });
            };
            if let Some(oldest_terminal) = state.admitted_key_order.remove(terminal_index) {
                state.admitted_keys.remove(&oldest_terminal);
            }
        }
        let key = key.to_owned();
        state.admitted_keys.insert(
            key.clone(),
            AdmittedAction {
                action_id: action_id.unwrap_or_default(),
                terminal: None,
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

    fn mark_terminal(&self, receipt: &ActionReceipt, terminal: AdmittedTerminal) {
        let (Some(key), Some(action_id)) = (&receipt.idempotency_key, receipt.action_id) else {
            return;
        };
        let mut state = lock_arbiter_state(&self.state);
        if let Some(admitted) = state.admitted_keys.get_mut(key)
            && admitted.action_id == action_id
        {
            admitted.terminal = Some(terminal);
        }
    }

    fn release_reservation(&self, receipt: &ActionReceipt) {
        let (Some(key), Some(action_id)) = (&receipt.idempotency_key, receipt.action_id) else {
            return;
        };
        let mut state = lock_arbiter_state(&self.state);
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

    pub(crate) fn terminal_outcome(
        &self,
        idempotency_key: &str,
        original_action_id: ActionId,
    ) -> Option<AdmittedTerminal> {
        lock_arbiter_state(&self.state)
            .admitted_keys
            .get(idempotency_key)
            .filter(|admitted| admitted.action_id == original_action_id)
            .and_then(|admitted| admitted.terminal)
    }

    /// Dispatches using the current wall clock.
    pub async fn dispatch(&self, action: ProposedAction, port: &dyn ActionPort) -> ActionResult {
        self.dispatch_at(action, port, Utc::now()).await
    }

    /// Dispatches using the current wall clock with a bounded host execution
    /// budget. A timeout after admission is returned as an indeterminate
    /// outcome, and the reservation guard prevents the same action from being
    /// replayed while its platform result is unknown.
    pub async fn dispatch_with_timeout(
        &self,
        action: ProposedAction,
        port: &dyn ActionPort,
        timeout: Duration,
    ) -> ActionResult {
        self.dispatch_at_with_timeout(action, port, Utc::now(), Some(timeout))
            .await
    }

    /// Deterministic dispatch entry point used by tests and schedulers.
    pub async fn dispatch_at(
        &self,
        action: ProposedAction,
        port: &dyn ActionPort,
        now: DateTime<Utc>,
    ) -> ActionResult {
        self.dispatch_at_with_timeout(action, port, now, None).await
    }

    async fn dispatch_at_with_timeout(
        &self,
        action: ProposedAction,
        port: &dyn ActionPort,
        now: DateTime<Utc>,
        execution_timeout: Option<Duration>,
    ) -> ActionResult {
        // Reject malformed, stale, unauthorized, or unsupported actions before
        // consulting a host resolver. This keeps validation and authorization
        // independent from target availability and avoids leaking resolver
        // information to callers that could not execute the action anyway.
        if let Err(rejection) = self.validate_at(&action, now) {
            return ActionResult::Rejected(rejection);
        }
        // The standalone CLI intentionally uses a tiny non-Tokio executor.
        // Keep that host compatible; production hosts with a Tokio reactor get
        // the bounded resolver + adapter budget below.
        let execution_deadline = execution_timeout
            .filter(|_| tokio::runtime::Handle::try_current().is_ok())
            .map(|timeout| (Instant::now() + timeout, timeout));
        if let ProposedAction::ReachOut(reach_out) = &action
            && let Some(resolver) = &self.delivery_resolver
        {
            let resolution = match execution_deadline {
                Some((deadline, timeout)) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match tokio::time::timeout(remaining, resolver.resolve(reach_out.person_id))
                        .await
                    {
                        Ok(resolution) => resolution,
                        Err(_) => {
                            return ActionResult::Rejected(
                                ActionRejection::DeliveryResolutionFailed {
                                    action_id: Some(reach_out.action_id()),
                                    error: format!(
                                        "delivery resolution timed out after {}ms",
                                        timeout.as_millis()
                                    ),
                                },
                            );
                        }
                    }
                }
                None => resolver.resolve(reach_out.person_id).await,
            };
            match resolution {
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
        let mut cancellation_guard = DispatchReservationGuard::new(self, &receipt);
        let execution = match execution_deadline {
            Some((deadline, timeout)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, port.execute(&action)).await {
                    Ok(execution) => execution,
                    Err(_) => {
                        // The adapter future has been cancelled. Conservatively
                        // retain the reservation as indeterminate; a caller can
                        // inspect the returned result without issuing a blind
                        // duplicate retry.
                        self.mark_terminal(&receipt, AdmittedTerminal::Indeterminate);
                        cancellation_guard.disarm();
                        return ActionResult::Executed {
                            receipt,
                            outcome: ActionPortOutcome::DeliveryIndeterminate {
                                reason: format!(
                                    "action execution timed out after {}ms",
                                    timeout.as_millis()
                                ),
                                conversation_id: match action.scope() {
                                    ActionScope::Conversation(conversation_id) => {
                                        Some(conversation_id)
                                    }
                                    ActionScope::Person(_) | ActionScope::Global => None,
                                },
                            },
                        };
                    }
                }
            }
            None => port.execute(&action).await,
        };
        // From this point on the result is available and no further await can
        // cancel the dispatch before its terminal state is recorded.
        cancellation_guard.disarm();
        match execution {
            Ok(
                outcome @ (ActionPortOutcome::Delivered { .. }
                | ActionPortOutcome::ToolCompleted { .. }),
            ) => {
                self.mark_terminal(&receipt, AdmittedTerminal::Succeeded);
                ActionResult::Executed { receipt, outcome }
            }
            Ok(outcome @ ActionPortOutcome::DeliveryIndeterminate { .. }) => {
                self.mark_terminal(&receipt, AdmittedTerminal::Indeterminate);
                ActionResult::Executed { receipt, outcome }
            }
            Ok(outcome @ ActionPortOutcome::ToolFailed { .. }) => {
                self.mark_terminal(&receipt, AdmittedTerminal::Failed);
                ActionResult::Executed { receipt, outcome }
            }
            Ok(outcome @ ActionPortOutcome::Deferred { .. }) => {
                self.release_reservation(&receipt);
                ActionResult::Executed { receipt, outcome }
            }
            Err(error) => {
                if error.retryable {
                    self.release_reservation(&receipt);
                } else {
                    self.mark_terminal(&receipt, AdmittedTerminal::Failed);
                }
                ActionResult::Failed { receipt, error }
            }
        }
    }
}

fn capability_for(action: &ProposedAction) -> Option<ActionCapability> {
    match action {
        ProposedAction::SendMessage(_) => Some(ActionCapability::SendMessage),
        ProposedAction::ReachOut(_) => Some(ActionCapability::ReachOut),
        ProposedAction::UseTool(_) => Some(ActionCapability::UseTool),
        ProposedAction::CreateOpenLoop(_) => Some(ActionCapability::CreateOpenLoop),
        ProposedAction::ResolveOpenLoop(_) => Some(ActionCapability::ResolveOpenLoop),
        ProposedAction::StartGoal(_) => Some(ActionCapability::StartGoal),
        ProposedAction::CancelGoal(_) => Some(ActionCapability::CancelGoal),
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
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

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
    fn an_undeclared_tool_is_refused_and_a_declared_one_respects_the_ceiling() {
        // The `UseTool` capability only says this host exposes tools. A call
        // names a specific tool, so the host has to declare that tool — and a
        // name nobody declared must not reach the environment on the strength
        // of the capability alone.
        let conversation_id = ConversationId::new();
        let scope = ActionScope::Conversation(conversation_id);
        let tool_action = |name: &str| {
            ProposedAction::UseTool(
                crate::ToolAction::new(name, "{}", scope).expect("valid tool action"),
            )
        };
        let mut capabilities = EnvironmentCapabilities::all();
        capabilities.actions.push(ActionDescriptor::tool(
            "web.search",
            EffectScope::ReadOnly,
            true,
        ));
        capabilities.actions.push(ActionDescriptor::tool(
            "memory.remember",
            EffectScope::UserScoped,
            false,
        ));
        capabilities.actions.push(ActionDescriptor::tool(
            "group.message.send",
            EffectScope::Outbound,
            false,
        ));

        let arbiter =
            ActionArbiter::new(ActionArbiterConfig::default().with_capabilities(capabilities));

        // Declared and within the default ceiling: admitted.
        arc_ok(
            &arbiter,
            &tool_action("web.search"),
            "declared read-only tool",
        );
        arc_ok(
            &arbiter,
            &tool_action("group.message.send"),
            "declared outbound tool",
        );

        // Never declared: refused, whatever the capability says.
        let refusal = arbiter
            .validate_at(&tool_action("made.up.tool"), Utc::now())
            .expect_err("an undeclared tool must be refused");
        assert!(
            matches!(&refusal, ActionRejection::Unauthorized { reason, .. }
                if reason.contains("not declared")),
            "unexpected refusal: {refusal:?}"
        );
    }

    #[test]
    fn a_turn_ceiling_refuses_a_tool_whose_effects_reach_further() {
        let conversation_id = ConversationId::new();
        let scope = ActionScope::Conversation(conversation_id);
        let mut capabilities = EnvironmentCapabilities::all();
        capabilities.actions.push(ActionDescriptor::tool(
            "web.search",
            EffectScope::ReadOnly,
            true,
        ));
        capabilities.actions.push(ActionDescriptor::tool(
            "memory.remember",
            EffectScope::UserScoped,
            false,
        ));
        capabilities.actions.push(ActionDescriptor::tool(
            "group.message.send",
            EffectScope::Outbound,
            false,
        ));

        // A turn that took in text written by someone else may still read and
        // may still change the acting person's own state — but it must not
        // speak in her name.
        let arbiter = ActionArbiter::new(ActionArbiterConfig {
            capabilities,
            effect_ceiling: EffectScope::UserScoped,
            ..ActionArbiterConfig::default()
        });
        let action = |name: &str| {
            ProposedAction::UseTool(
                crate::ToolAction::new(name, "{}", scope).expect("valid tool action"),
            )
        };
        arc_ok(&arbiter, &action("web.search"), "read-only stays allowed");
        arc_ok(
            &arbiter,
            &action("memory.remember"),
            "user-scoped stays allowed",
        );
        let refusal = arbiter
            .validate_at(&action("group.message.send"), Utc::now())
            .expect_err("an outbound tool must exceed a user-scoped ceiling");
        assert!(
            matches!(&refusal, ActionRejection::Unauthorized { reason, .. }
                if reason.contains("beyond this turn's")),
            "unexpected refusal: {refusal:?}"
        );
    }

    /// Asserts a proposed action is admitted, with a readable failure.
    fn arc_ok(arbiter: &ActionArbiter, action: &ProposedAction, what: &str) {
        if let Err(rejection) = arbiter.validate_at(action, Utc::now()) {
            panic!("{what} should be admitted, got {rejection:?}");
        }
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
    fn environment_all_exposes_every_platform_neutral_action_capability() {
        let capabilities = EnvironmentCapabilities::all();
        for capability in [
            ActionCapability::SendMessage,
            ActionCapability::ReachOut,
            ActionCapability::UseTool,
            ActionCapability::CreateOpenLoop,
            ActionCapability::ResolveOpenLoop,
            ActionCapability::StartGoal,
            ActionCapability::CancelGoal,
        ] {
            assert!(capabilities.supports(capability, ActionScope::Global));
        }
        // `StartCall` 不在里面是**决定**，不是漏了：打电话要走宿主自己的带外语音
        // 通道（QQ 那边是 NapCat AV 桥），能说话不等于能拨号。要拨号的宿主必须自己
        // 显式声明，否则就是宣称一条它其实开不了的通道。
        assert!(
            !capabilities.supports(ActionCapability::StartCall, ActionScope::Global),
            "StartCall 需要带外语音通道，不该由 platform-neutral 的 all() 提供"
        );
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
            ProposedAction::ReachOut(_)
            | ProposedAction::UseTool(_)
            | ProposedAction::CreateOpenLoop(_)
            | ProposedAction::ResolveOpenLoop(_)
            | ProposedAction::StartGoal(_)
            | ProposedAction::CancelGoal(_)
            | ProposedAction::Noop => unreachable!(),
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
    fn expired_scope_cooldowns_are_reclaimed_before_the_capacity_check() {
        // `last_by_scope` 只增不删：冷却早已过去的条目永远占着名额，4096 个会话
        // 之后每个**新**会话都会被拒，报的还是"幂等状态已满"。
        let now = Utc::now();
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default()
                .with_capabilities(EnvironmentCapabilities::all())
                .with_cooldown(std::time::Duration::from_secs(60)),
        );
        {
            let mut state = lock_arbiter_state(&arbiter.state);
            let expired = now - ChronoDuration::hours(1);
            for _ in 0..MAX_TRACKED_ACTION_SCOPES {
                state
                    .last_by_scope
                    .insert(ActionScope::Conversation(ConversationId::new()), expired);
            }
        }
        let fresh = ConversationId::new();
        arbiter
            .admit_at(&send_with_key(fresh, "fresh-scope", now), now)
            .expect("冷却已过期的名额应当被回收，而不是把新会话拒之门外");
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
            arbiter.mark_terminal(&receipt, AdmittedTerminal::Succeeded);
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

    struct IndeterminatePort {
        calls: AtomicUsize,
    }

    impl ActionPort for IndeterminatePort {
        fn execute<'a>(&'a self, _action: &'a ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ActionPortOutcome::DeliveryIndeterminate {
                    reason: "transport outcome unknown".to_owned(),
                    conversation_id: None,
                })
            })
        }
    }

    struct FailingPort {
        calls: AtomicUsize,
        retryable: bool,
    }

    impl ActionPort for FailingPort {
        fn execute<'a>(&'a self, _action: &'a ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let retryable = self.retryable;
            Box::pin(async move { Err(ActionPortError::new("adapter_failure", retryable)) })
        }
    }

    struct PendingPort {
        entered: Arc<Notify>,
    }

    impl ActionPort for PendingPort {
        fn execute<'a>(&'a self, _action: &'a ProposedAction) -> ActionPortFuture<'a> {
            let entered = Arc::clone(&self.entered);
            Box::pin(async move {
                entered.notify_one();
                pending::<Result<ActionPortOutcome, ActionPortError>>().await
            })
        }
    }

    #[tokio::test]
    async fn cancellation_after_admission_marks_the_reservation_indeterminate() {
        let now = Utc::now();
        let action = send_with_key(ConversationId::new(), "cancelled-dispatch", now);
        let action_id = action.action_id().expect("message action has an id");
        let arbiter = Arc::new(ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        ));
        let entered = Arc::new(Notify::new());
        let port = Arc::new(PendingPort {
            entered: Arc::clone(&entered),
        });
        let dispatch_arbiter = Arc::clone(&arbiter);
        let dispatch_action = action.clone();
        let dispatch_port = Arc::clone(&port);
        let dispatch = tokio::spawn(async move {
            dispatch_arbiter
                .dispatch_at(dispatch_action, dispatch_port.as_ref(), now)
                .await
        });

        entered.notified().await;
        dispatch.abort();
        assert!(
            dispatch
                .await
                .expect_err("the pending dispatch should be cancelled")
                .is_cancelled()
        );
        assert_eq!(
            arbiter.terminal_outcome("cancelled-dispatch", action_id),
            Some(AdmittedTerminal::Indeterminate)
        );
        let replay_port = FakePort {
            calls: AtomicUsize::new(0),
        };
        assert!(matches!(
            arbiter.dispatch_at(action, &replay_port, now).await,
            ActionResult::Rejected(ActionRejection::Duplicate {
                original_action_id,
                ..
            }) if original_action_id == action_id
        ));
        assert_eq!(replay_port.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn indeterminate_delivery_is_terminal_without_being_successful() {
        let now = Utc::now();
        let action = send_with_key(ConversationId::new(), "terminal-unknown", now);
        let action_id = action.action_id().expect("message action has an id");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let port = IndeterminatePort {
            calls: AtomicUsize::new(0),
        };

        let first = arbiter.dispatch_at(action.clone(), &port, now).await;
        assert!(matches!(
            first,
            ActionResult::Executed {
                outcome: ActionPortOutcome::DeliveryIndeterminate { .. },
                ..
            }
        ));
        assert!(!first.is_success());
        assert_eq!(
            arbiter.terminal_outcome("terminal-unknown", action_id),
            Some(AdmittedTerminal::Indeterminate)
        );
        assert!(matches!(
            arbiter.dispatch_at(action, &port, now).await,
            ActionResult::Rejected(ActionRejection::Duplicate { .. })
        ));
        assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn only_retryable_port_errors_release_the_idempotency_reservation() {
        let now = Utc::now();
        let action = send_with_key(ConversationId::new(), "port-error-policy", now);
        let action_id = action.action_id().expect("message action has an id");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let terminal = FailingPort {
            calls: AtomicUsize::new(0),
            retryable: false,
        };

        assert!(matches!(
            arbiter.dispatch_at(action.clone(), &terminal, now).await,
            ActionResult::Failed {
                error: ActionPortError {
                    retryable: false,
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            arbiter.terminal_outcome("port-error-policy", action_id),
            Some(AdmittedTerminal::Failed)
        );
        assert!(matches!(
            arbiter.dispatch_at(action.clone(), &terminal, now).await,
            ActionResult::Rejected(ActionRejection::Duplicate { .. })
        ));
        assert_eq!(terminal.calls.load(Ordering::SeqCst), 1);

        let retryable_action =
            send_with_key(ConversationId::new(), "retryable-port-error-policy", now);
        let retryable = FailingPort {
            calls: AtomicUsize::new(0),
            retryable: true,
        };
        assert!(matches!(
            arbiter
                .dispatch_at(retryable_action.clone(), &retryable, now)
                .await,
            ActionResult::Failed {
                error: ActionPortError {
                    retryable: true,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            arbiter.dispatch_at(retryable_action, &retryable, now).await,
            ActionResult::Failed { .. }
        ));
        assert_eq!(retryable.calls.load(Ordering::SeqCst), 2);
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

    struct PendingResolver;

    impl DeliveryResolver for PendingResolver {
        fn resolve<'a>(&'a self, _person_id: PersonId) -> DeliveryResolverFuture<'a> {
            Box::pin(pending())
        }
    }

    #[tokio::test]
    async fn bounded_dispatch_times_out_delivery_resolution_before_admission() {
        let person = PersonId::new();
        let action = ProposedAction::ReachOut(
            ReachOutAction::new(
                person,
                MessageContent::text("hello"),
                ProactiveMotive::CheckIn,
            )
            .expect("action"),
        );
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        )
        .with_delivery_resolver(Arc::new(PendingResolver));
        let port = FakePort {
            calls: AtomicUsize::new(0),
        };

        assert!(matches!(
            arbiter
                .dispatch_with_timeout(action.clone(), &port, Duration::from_millis(10))
                .await,
            ActionResult::Rejected(ActionRejection::DeliveryResolutionFailed {
                error,
                ..
            }) if error.contains("timed out")
        ));
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);

        let receipt = arbiter
            .admit_at(&action, Utc::now())
            .expect("resolver timeout must happen before idempotency admission");
        arbiter.release_reservation(&receipt);
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
