//! EnvironmentState: host availability, tool health, runtime load (v4 §73–79,
//! §183). Host state belongs to the environment, not to SelfModel.

use super::{
    WorldValidationError,
    common::{dedupe, validate_text},
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_HOST_ID_BYTES: usize = 64;
pub const MAX_HOST_ID_CHARS: usize = 64;
pub const MAX_TOOL_NAME_BYTES: usize = 128;
pub const MAX_TOOL_NAME_CHARS: usize = 64;
pub const MAX_ENVIRONMENT_HOSTS: usize = 32;
pub const MAX_ENVIRONMENT_TOOLS: usize = 128;

/// Platform-neutral host identifier ("qq", "cli", "desktop", ...). Providers
/// stay opaque to the core (v4 §165).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    pub fn new(value: impl Into<String>) -> Result<Self, WorldValidationError> {
        let value = value.into();
        if value.is_empty() {
            return Err(WorldValidationError::Empty { field: "host id" });
        }
        if value.len() > MAX_HOST_ID_BYTES || value.chars().count() > MAX_HOST_ID_CHARS {
            return Err(WorldValidationError::TooLong {
                field: "host id",
                length: value.len(),
                maximum: MAX_HOST_ID_BYTES,
            });
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(WorldValidationError::InvalidState {
                reason: "host id must be [a-z0-9_-]",
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for HostId {
    type Err = WorldValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// Health of one service (host, tool, model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceHealth {
    Healthy,
    Degraded,
    Unavailable,
    Unknown,
}

/// A host's availability estimate with TTL (v4 §75, §200).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostState {
    host: HostId,
    health: ServiceHealth,
    observed_at: DateTime<Utc>,
    ttl: Duration,
    version: u64,
}

impl HostState {
    pub fn new(
        host: HostId,
        health: ServiceHealth,
        observed_at: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<Self, WorldValidationError> {
        let state = Self {
            host,
            health,
            observed_at,
            ttl,
            version: 1,
        };
        state.validate()?;
        Ok(state)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.ttl <= Duration::zero() {
            return Err(WorldValidationError::InvalidState {
                reason: "host state TTL must be positive",
            });
        }
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        Ok(())
    }

    #[must_use]
    pub fn host(&self) -> &HostId {
        &self.host
    }

    #[must_use]
    pub const fn health(&self) -> ServiceHealth {
        self.health
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Effective health at `now`: Unknown once the TTL lapses (v4 §200).
    #[must_use]
    pub fn effective_health_at(&self, now: DateTime<Utc>) -> ServiceHealth {
        if now < self.observed_at {
            return ServiceHealth::Unknown;
        }
        if now > self.observed_at + self.ttl {
            return ServiceHealth::Unknown;
        }
        self.health
    }
}

/// Health of one tool with optional last failure detail (v4 §79, §141).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolHealth {
    tool_name: String,
    health: ServiceHealth,
    detail: Option<String>,
    observed_at: DateTime<Utc>,
    ttl: Duration,
    version: u64,
}

impl ToolHealth {
    pub fn new(
        tool_name: impl Into<String>,
        health: ServiceHealth,
        detail: Option<impl Into<String>>,
        observed_at: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<Self, WorldValidationError> {
        let tool = Self {
            tool_name: validate_text(tool_name, "tool name")?,
            health,
            detail: match detail {
                Some(detail) => Some(validate_text(detail, "tool health detail")?),
                None => None,
            },
            observed_at,
            ttl,
            version: 1,
        };
        tool.validate()?;
        Ok(tool)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_text(self.tool_name.clone(), "tool name")?;
        if self.tool_name.len() > MAX_TOOL_NAME_BYTES
            || self.tool_name.chars().count() > MAX_TOOL_NAME_CHARS
        {
            return Err(WorldValidationError::TooLong {
                field: "tool name",
                length: self.tool_name.len(),
                maximum: MAX_TOOL_NAME_BYTES,
            });
        }
        if let Some(detail) = &self.detail {
            validate_text(detail.clone(), "tool health detail")?;
        }
        if self.ttl <= Duration::zero() {
            return Err(WorldValidationError::InvalidState {
                reason: "tool health TTL must be positive",
            });
        }
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        Ok(())
    }

    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    #[must_use]
    pub const fn health(&self) -> ServiceHealth {
        self.health
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn effective_health_at(&self, now: DateTime<Utc>) -> ServiceHealth {
        if now < self.observed_at {
            return ServiceHealth::Unknown;
        }
        if now > self.observed_at + self.ttl {
            return ServiceHealth::Unknown;
        }
        self.health
    }
}

/// Runtime load observed by the environment (v4 §78).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RuntimeLoad {
    model_queue_depth: u32,
    tool_latency_ms: Option<u64>,
    available_hosts: u32,
    total_hosts: u32,
    observed_at: DateTime<Utc>,
}

impl RuntimeLoad {
    pub fn new(
        model_queue_depth: u32,
        tool_latency_ms: Option<u64>,
        available_hosts: u32,
        total_hosts: u32,
        observed_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        if total_hosts > 0 && available_hosts > total_hosts {
            return Err(WorldValidationError::InvalidState {
                reason: "available hosts exceed total hosts",
            });
        }
        Ok(Self {
            model_queue_depth,
            tool_latency_ms,
            available_hosts,
            total_hosts,
            observed_at,
        })
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.total_hosts > 0 && self.available_hosts > self.total_hosts {
            return Err(WorldValidationError::InvalidState {
                reason: "available hosts exceed total hosts",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn model_queue_depth(&self) -> u32 {
        self.model_queue_depth
    }

    #[must_use]
    pub const fn tool_latency_ms(&self) -> Option<u64> {
        self.tool_latency_ms
    }

    #[must_use]
    pub const fn available_hosts(&self) -> u32 {
        self.available_hosts
    }

    #[must_use]
    pub const fn total_hosts(&self) -> u32 {
        self.total_hosts
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    /// 0..1 fraction of hosts currently available.
    #[must_use]
    pub fn availability_fraction(&self) -> f32 {
        if self.total_hosts == 0 {
            0.0
        } else {
            (self.available_hosts as f32 / self.total_hosts as f32).clamp(0.0, 1.0)
        }
    }
}

/// A proposed environment refresh (v4 §141–§144).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentUpdate {
    hosts: Vec<HostState>,
    tools: Vec<ToolHealth>,
    model_health: ServiceHealth,
    load: RuntimeLoad,
}

impl EnvironmentUpdate {
    pub fn new(
        hosts: Vec<HostState>,
        tools: Vec<ToolHealth>,
        model_health: ServiceHealth,
        load: RuntimeLoad,
    ) -> Result<Self, WorldValidationError> {
        let update = Self {
            hosts: dedupe(hosts, "environment hosts", false)?,
            tools: dedupe(tools, "environment tools", false)?,
            model_health,
            load,
        };
        update.validate()?;
        Ok(update)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.hosts.len() > MAX_ENVIRONMENT_HOSTS {
            return Err(WorldValidationError::TooManyItems {
                field: "environment hosts",
                length: self.hosts.len(),
                maximum: MAX_ENVIRONMENT_HOSTS,
            });
        }
        if self.tools.len() > MAX_ENVIRONMENT_TOOLS {
            return Err(WorldValidationError::TooManyItems {
                field: "environment tools",
                length: self.tools.len(),
                maximum: MAX_ENVIRONMENT_TOOLS,
            });
        }
        for host in &self.hosts {
            host.validate()?;
        }
        for tool in &self.tools {
            tool.validate()?;
        }
        self.load.validate()
    }

    #[must_use]
    pub fn hosts(&self) -> &[HostState] {
        &self.hosts
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolHealth] {
        &self.tools
    }

    #[must_use]
    pub const fn model_health(&self) -> ServiceHealth {
        self.model_health
    }

    #[must_use]
    pub const fn load(&self) -> RuntimeLoad {
        self.load
    }
}

/// Environment state (v4 §74).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentState {
    hosts: Vec<HostState>,
    tools: Vec<ToolHealth>,
    model_health: ServiceHealth,
    load: RuntimeLoad,
    version: u64,
}

impl Default for EnvironmentState {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            tools: Vec::new(),
            model_health: ServiceHealth::Unknown,
            load: RuntimeLoad::new(0, None, 0, 0, Utc::now()).expect("empty load is valid"),
            version: 1,
        }
    }
}

impl EnvironmentState {
    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.hosts.len() > MAX_ENVIRONMENT_HOSTS {
            return Err(WorldValidationError::TooManyItems {
                field: "environment hosts",
                length: self.hosts.len(),
                maximum: MAX_ENVIRONMENT_HOSTS,
            });
        }
        if self.tools.len() > MAX_ENVIRONMENT_TOOLS {
            return Err(WorldValidationError::TooManyItems {
                field: "environment tools",
                length: self.tools.len(),
                maximum: MAX_ENVIRONMENT_TOOLS,
            });
        }
        for host in &self.hosts {
            host.validate()?;
        }
        for tool in &self.tools {
            tool.validate()?;
        }
        self.load.validate()?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        Ok(())
    }

    #[must_use]
    pub fn hosts(&self) -> &[HostState] {
        &self.hosts
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolHealth] {
        &self.tools
    }

    #[must_use]
    pub const fn model_health(&self) -> ServiceHealth {
        self.model_health
    }

    #[must_use]
    pub const fn load(&self) -> RuntimeLoad {
        self.load
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Effective health of one host right now (TTL-aware).
    #[must_use]
    pub fn host_health_at(&self, host: &HostId, now: DateTime<Utc>) -> ServiceHealth {
        self.hosts
            .iter()
            .find(|state| state.host() == host)
            .map_or(ServiceHealth::Unknown, |state| state.effective_health_at(now))
    }

    /// Effective health of one tool right now (TTL-aware).
    #[must_use]
    pub fn tool_health_at(&self, tool_name: &str, now: DateTime<Utc>) -> ServiceHealth {
        self.tools
            .iter()
            .find(|health| health.tool_name() == tool_name)
            .map_or(ServiceHealth::Unknown, |health| health.effective_health_at(now))
    }

    /// Merge a validated update: refresh hosts/tools/model/load (v4 §141).
    pub fn apply(&self, update: EnvironmentUpdate) -> Result<Self, WorldValidationError> {
        update.validate()?;
        let mut environment = self.clone();
        for host in update.hosts() {
            if let Some(existing) = environment
                .hosts
                .iter_mut()
                .find(|existing| existing.host() == host.host())
            {
                *existing = host.clone();
            } else {
                environment.hosts.push(host.clone());
            }
        }
        for tool in update.tools() {
            if let Some(existing) = environment
                .tools
                .iter_mut()
                .find(|existing| existing.tool_name() == tool.tool_name())
            {
                *existing = tool.clone();
            } else {
                environment.tools.push(tool.clone());
            }
        }
        environment.model_health = update.model_health();
        environment.load = update.load();
        environment.version = environment.version.saturating_add(1);
        environment.validate()?;
        Ok(environment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn host(id: &str, health: ServiceHealth, now: DateTime<Utc>) -> HostState {
        HostState::new(
            HostId::new(id).expect("host"),
            health,
            now,
            Duration::minutes(10),
        )
        .expect("host state")
    }

    #[test]
    fn host_ttl_expires_to_unknown() {
        let now = Utc::now();
        let state = host("qq", ServiceHealth::Healthy, now);
        assert_eq!(state.effective_health_at(now + Duration::minutes(5)), ServiceHealth::Healthy);
        // TTL (10 min) lapsed → Unknown, not still "online" (v4 §200).
        assert_eq!(
            state.effective_health_at(now + Duration::minutes(11)),
            ServiceHealth::Unknown
        );
    }

    #[test]
    fn environment_merges_and_validates() {
        let now = Utc::now();
        let mut environment = EnvironmentState::default();
        let update = EnvironmentUpdate::new(
            vec![host("qq", ServiceHealth::Healthy, now), host("cli", ServiceHealth::Unavailable, now)],
            vec![ToolHealth::new(
                "web_fetch",
                ServiceHealth::Degraded,
                Some("429 rate limited"),
                now,
                Duration::minutes(5),
            )
            .expect("tool")],
            ServiceHealth::Healthy,
            RuntimeLoad::new(3, Some(120), 1, 2, now).expect("load"),
        )
        .expect("update");
        environment = environment.apply(update).expect("applied");
        assert_eq!(environment.hosts().len(), 2);
        assert_eq!(
            environment.tool_health_at("web_fetch", now + Duration::seconds(1)),
            ServiceHealth::Degraded
        );
        assert_eq!(environment.tool_health_at("web_fetch", now + Duration::minutes(6)), ServiceHealth::Unknown);
        assert_eq!(environment.host_health_at(&HostId::new("qq").expect("id"), now), ServiceHealth::Healthy);
        assert_eq!(environment.load().availability_fraction(), 0.5);
        assert_eq!(environment.version(), 2);
    }

    #[test]
    fn host_id_is_narrow_and_validated() {
        assert!(HostId::new("qq").is_ok());
        assert!(HostId::new("desktop_app").is_ok());
        assert!(HostId::new("QQ").is_err());
        assert!(HostId::new("").is_err());
        assert!(HostId::new("x".repeat(MAX_HOST_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn invalid_environment_state_is_rejected() {
        let now = Utc::now();
        let mut tool = ToolHealth::new("tool", ServiceHealth::Healthy, None::<&str>, now, Duration::minutes(1))
            .expect("tool");
        tool.tool_name = "t".repeat(MAX_TOOL_NAME_BYTES + 1);
        let result = EnvironmentUpdate::new(
            vec![],
            vec![tool],
            ServiceHealth::Healthy,
            RuntimeLoad::new(0, None, 0, 0, now).expect("load"),
        );
        // The invalid tool makes construction fail (validate inside new()).
        assert!(result.is_err());
    }
}
