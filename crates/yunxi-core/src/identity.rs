use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

pub const MAX_PLATFORM_ID_BYTES: usize = 64;
pub const MAX_EXTERNAL_ID_BYTES: usize = 512;
pub const MAX_CONVERSATION_MEMBER_ROLE_BYTES: usize = 128;
pub const MAX_CONVERSATION_MEMBER_ROLE_CHARS: usize = 64;

macro_rules! domain_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            #[must_use]
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self::from_uuid(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.into_uuid()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self::from_uuid)
            }
        }
    };
}

domain_id!(PersonId);
domain_id!(ConversationId);
domain_id!(MessageId);
domain_id!(EventId);
domain_id!(GoalId);
domain_id!(OpenLoopId);
domain_id!(MemoryId);

/// Stable identifier for a platform or identity provider.
///
/// Platform IDs use the deliberately narrow `[a-z][a-z0-9_-]*` format so
/// storage adapters can compare them consistently. External identifiers stay
/// opaque and are not interpreted by Core.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PlatformId(String);

impl PlatformId {
    pub fn new(value: impl Into<String>) -> Result<Self, ExternalReferenceError> {
        let value = value.into();
        validate_platform_id(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for PlatformId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PlatformId {
    type Err = ExternalReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for PlatformId {
    type Error = ExternalReferenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for PlatformId {
    type Error = ExternalReferenceError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for PlatformId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ExternalIdentity {
    platform: PlatformId,
    external_id: String,
}

impl ExternalIdentity {
    pub fn new(
        platform: PlatformId,
        external_id: impl Into<String>,
    ) -> Result<Self, ExternalReferenceError> {
        let external_id = external_id.into();
        validate_external_id(&external_id)?;
        Ok(Self {
            platform,
            external_id,
        })
    }

    #[must_use]
    pub const fn platform(&self) -> &PlatformId {
        &self.platform
    }

    #[must_use]
    pub fn external_id(&self) -> &str {
        &self.external_id
    }

    #[must_use]
    pub fn into_parts(self) -> (PlatformId, String) {
        (self.platform, self.external_id)
    }
}

impl<'de> Deserialize<'de> for ExternalIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SerializedExternalIdentity {
            platform: PlatformId,
            external_id: String,
        }

        let value = SerializedExternalIdentity::deserialize(deserializer)?;
        Self::new(value.platform, value.external_id).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    Group,
    System,
}

impl ConversationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
            Self::System => "system",
        }
    }
}

impl fmt::Display for ConversationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ConversationKind {
    type Err = ExternalReferenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "direct" => Ok(Self::Direct),
            "group" => Ok(Self::Group),
            "system" => Ok(Self::System),
            _ => Err(ExternalReferenceError::UnknownConversationKind {
                value: value.to_owned(),
            }),
        }
    }
}

/// A lazily discovered relationship between a canonical person and a
/// canonical conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationMember {
    conversation_id: ConversationId,
    person_id: PersonId,
    role: Option<String>,
}

impl ConversationMember {
    #[must_use]
    pub const fn new(conversation_id: ConversationId, person_id: PersonId) -> Self {
        Self {
            conversation_id,
            person_id,
            role: None,
        }
    }

    pub fn with_role(
        mut self,
        role: Option<String>,
    ) -> Result<Self, ConversationMemberValidationError> {
        if let Some(role) = &role {
            validate_conversation_member_role(role)?;
        }
        self.role = role;
        Ok(self)
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub const fn person_id(&self) -> PersonId {
        self.person_id
    }

    #[must_use]
    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    pub fn validate(&self) -> Result<(), ConversationMemberValidationError> {
        if let Some(role) = &self.role {
            validate_conversation_member_role(role)?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ConversationMember {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            conversation_id: ConversationId,
            person_id: PersonId,
            role: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.conversation_id, wire.person_id)
            .with_role(wire.role)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConversationMemberValidationError {
    #[error("conversation member role must not be empty")]
    EmptyRole,
    #[error("conversation member role must not contain NUL")]
    RoleContainsNul,
    #[error("conversation member role is {length} bytes, above maximum {maximum}")]
    RoleTooLong { length: usize, maximum: usize },
    #[error("conversation member role is {length} characters, above maximum {maximum}")]
    RoleTooManyCharacters { length: usize, maximum: usize },
}

fn validate_conversation_member_role(role: &str) -> Result<(), ConversationMemberValidationError> {
    if role.trim().is_empty() {
        return Err(ConversationMemberValidationError::EmptyRole);
    }
    if role.contains('\0') {
        return Err(ConversationMemberValidationError::RoleContainsNul);
    }
    if role.len() > MAX_CONVERSATION_MEMBER_ROLE_BYTES {
        return Err(ConversationMemberValidationError::RoleTooLong {
            length: role.len(),
            maximum: MAX_CONVERSATION_MEMBER_ROLE_BYTES,
        });
    }
    let chars = role.chars().count();
    if chars > MAX_CONVERSATION_MEMBER_ROLE_CHARS {
        return Err(ConversationMemberValidationError::RoleTooManyCharacters {
            length: chars,
            maximum: MAX_CONVERSATION_MEMBER_ROLE_CHARS,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ExternalConversation {
    platform: PlatformId,
    external_id: String,
    kind: ConversationKind,
}

impl ExternalConversation {
    pub fn new(
        platform: PlatformId,
        external_id: impl Into<String>,
        kind: ConversationKind,
    ) -> Result<Self, ExternalReferenceError> {
        let external_id = external_id.into();
        validate_external_id(&external_id)?;
        Ok(Self {
            platform,
            external_id,
            kind,
        })
    }

    #[must_use]
    pub const fn platform(&self) -> &PlatformId {
        &self.platform
    }

    #[must_use]
    pub fn external_id(&self) -> &str {
        &self.external_id
    }

    #[must_use]
    pub const fn kind(&self) -> ConversationKind {
        self.kind
    }

    #[must_use]
    pub fn into_parts(self) -> (PlatformId, String, ConversationKind) {
        (self.platform, self.external_id, self.kind)
    }
}

impl<'de> Deserialize<'de> for ExternalConversation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SerializedExternalConversation {
            platform: PlatformId,
            external_id: String,
            kind: ConversationKind,
        }

        let value = SerializedExternalConversation::deserialize(deserializer)?;
        Self::new(value.platform, value.external_id, value.kind).map_err(serde::de::Error::custom)
    }
}

fn validate_platform_id(value: &str) -> Result<(), ExternalReferenceError> {
    if value.is_empty() {
        return Err(ExternalReferenceError::EmptyPlatformId);
    }
    if value.len() > MAX_PLATFORM_ID_BYTES {
        return Err(ExternalReferenceError::PlatformIdTooLong {
            length: value.len(),
            maximum: MAX_PLATFORM_ID_BYTES,
        });
    }

    let mut bytes = value.bytes();
    let starts_with_lowercase_ascii = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase());
    if !starts_with_lowercase_ascii
        || !bytes
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err(ExternalReferenceError::InvalidPlatformId);
    }
    Ok(())
}

fn validate_external_id(value: &str) -> Result<(), ExternalReferenceError> {
    if value.is_empty() {
        return Err(ExternalReferenceError::EmptyExternalId);
    }
    if value.len() > MAX_EXTERNAL_ID_BYTES {
        return Err(ExternalReferenceError::ExternalIdTooLong {
            length: value.len(),
            maximum: MAX_EXTERNAL_ID_BYTES,
        });
    }
    if value.contains('\0') {
        return Err(ExternalReferenceError::ExternalIdContainsNul);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExternalReferenceError {
    #[error("platform ID must not be empty")]
    EmptyPlatformId,
    #[error("platform ID is {length} bytes, above maximum {maximum}")]
    PlatformIdTooLong { length: usize, maximum: usize },
    #[error("platform ID must match [a-z][a-z0-9_-]*")]
    InvalidPlatformId,
    #[error("external ID must not be empty")]
    EmptyExternalId,
    #[error("external ID is {length} bytes, above maximum {maximum}")]
    ExternalIdTooLong { length: usize, maximum: usize },
    #[error("external ID must not contain NUL")]
    ExternalIdContainsNul,
    #[error("unknown conversation kind `{value}`")]
    UnknownConversationKind { value: String },
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationId, ConversationKind, ConversationMember, ConversationMemberValidationError,
        ExternalConversation, ExternalIdentity, ExternalReferenceError,
        MAX_CONVERSATION_MEMBER_ROLE_BYTES, MAX_CONVERSATION_MEMBER_ROLE_CHARS,
        MAX_EXTERNAL_ID_BYTES, MAX_PLATFORM_ID_BYTES, MessageId, PersonId, PlatformId,
    };
    use std::str::FromStr;

    #[test]
    fn ids_have_value_semantics_and_distinct_values() {
        let person = PersonId::new();
        let same_person = PersonId::from_uuid(person.into_uuid());

        assert_eq!(person, same_person);
        assert_ne!(person, PersonId::new());
        assert_ne!(ConversationId::new(), ConversationId::new());
        assert_ne!(MessageId::new(), MessageId::new());
    }

    #[test]
    fn ids_serialize_as_uuid_strings_and_round_trip() {
        let person = PersonId::new();
        let encoded = serde_json::to_string(&person).expect("id should serialize");

        assert_eq!(encoded, format!("\"{person}\""));
        assert_eq!(
            serde_json::from_str::<PersonId>(&encoded).expect("id should deserialize"),
            person
        );
    }

    #[test]
    fn platform_ids_accept_only_the_stable_storage_format() {
        let platform = PlatformId::new("yunxi_app").expect("valid platform");

        assert_eq!(platform.as_str(), "yunxi_app");
        assert_eq!(platform.to_string(), "yunxi_app");
        assert_eq!(
            PlatformId::new("x".repeat(MAX_PLATFORM_ID_BYTES))
                .expect("boundary fits")
                .as_str(),
            "x".repeat(MAX_PLATFORM_ID_BYTES)
        );
        assert_eq!(
            PlatformId::new(""),
            Err(ExternalReferenceError::EmptyPlatformId)
        );
        assert_eq!(
            PlatformId::new("QQ"),
            Err(ExternalReferenceError::InvalidPlatformId)
        );
        assert_eq!(
            PlatformId::new("1provider"),
            Err(ExternalReferenceError::InvalidPlatformId)
        );
        assert_eq!(
            PlatformId::new("x".repeat(MAX_PLATFORM_ID_BYTES + 1)),
            Err(ExternalReferenceError::PlatformIdTooLong {
                length: MAX_PLATFORM_ID_BYTES + 1,
                maximum: MAX_PLATFORM_ID_BYTES,
            })
        );
    }

    #[test]
    fn external_references_preserve_opaque_ids_and_validate_bounds() {
        let platform = PlatformId::new("provider").expect("valid platform");
        let identity = ExternalIdentity::new(platform.clone(), "  Case-Sensitive ID  ")
            .expect("opaque external ID");
        let conversation =
            ExternalConversation::new(platform, "conversation/key", ConversationKind::Direct)
                .expect("valid external conversation");

        assert_eq!(identity.external_id(), "  Case-Sensitive ID  ");
        assert_eq!(conversation.external_id(), "conversation/key");
        assert_eq!(conversation.kind(), ConversationKind::Direct);
        assert!(
            ExternalIdentity::new(
                PlatformId::new("provider").expect("valid platform"),
                "x".repeat(MAX_EXTERNAL_ID_BYTES)
            )
            .is_ok()
        );
        assert_eq!(
            ExternalIdentity::new(
                PlatformId::new("provider").expect("valid platform"),
                "x".repeat(MAX_EXTERNAL_ID_BYTES + 1)
            ),
            Err(ExternalReferenceError::ExternalIdTooLong {
                length: MAX_EXTERNAL_ID_BYTES + 1,
                maximum: MAX_EXTERNAL_ID_BYTES,
            })
        );
        assert_eq!(
            ExternalConversation::new(
                PlatformId::new("provider").expect("valid platform"),
                "bad\0id",
                ConversationKind::Group,
            ),
            Err(ExternalReferenceError::ExternalIdContainsNul)
        );
    }

    #[test]
    fn invalid_external_references_cannot_enter_through_serde() {
        assert!(serde_json::from_str::<PlatformId>("\"Uppercase\"").is_err());
        assert!(
            serde_json::from_str::<ExternalIdentity>(r#"{"platform":"provider","external_id":""}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<ExternalConversation>(
                r#"{"platform":"provider","external_id":"conversation\u0000id","kind":"group"}"#
            )
            .is_err()
        );

        let identity = ExternalIdentity::new(
            PlatformId::new("provider").expect("valid platform"),
            "opaque-id",
        )
        .expect("valid identity");
        let encoded = serde_json::to_string(&identity).expect("identity should serialize");
        assert_eq!(
            serde_json::from_str::<ExternalIdentity>(&encoded)
                .expect("identity should deserialize"),
            identity
        );
    }

    #[test]
    fn conversation_kind_has_stable_storage_strings() {
        for (kind, stored) in [
            (ConversationKind::Direct, "direct"),
            (ConversationKind::Group, "group"),
            (ConversationKind::System, "system"),
        ] {
            assert_eq!(kind.as_str(), stored);
            assert_eq!(kind.to_string(), stored);
            assert_eq!(ConversationKind::from_str(stored), Ok(kind));
        }
        assert_eq!(
            ConversationKind::from_str("channel"),
            Err(ExternalReferenceError::UnknownConversationKind {
                value: "channel".to_owned(),
            })
        );
    }

    #[test]
    fn conversation_members_are_platform_neutral_and_validated() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let member = ConversationMember::new(conversation_id, person_id)
            .with_role(Some("moderator".to_owned()))
            .expect("valid role");

        assert_eq!(member.conversation_id(), conversation_id);
        assert_eq!(member.person_id(), person_id);
        assert_eq!(member.role(), Some("moderator"));
        let encoded = serde_json::to_string(&member).expect("serialize member");
        assert_eq!(
            serde_json::from_str::<ConversationMember>(&encoded).expect("deserialize member"),
            member
        );

        assert_eq!(
            ConversationMember::new(conversation_id, person_id)
                .with_role(Some(" ".to_owned()))
                .expect_err("blank role must fail"),
            ConversationMemberValidationError::EmptyRole
        );
        assert_eq!(
            ConversationMember::new(conversation_id, person_id)
                .with_role(Some("x".repeat(MAX_CONVERSATION_MEMBER_ROLE_BYTES + 1)))
                .expect_err("role is bounded"),
            ConversationMemberValidationError::RoleTooLong {
                length: MAX_CONVERSATION_MEMBER_ROLE_BYTES + 1,
                maximum: MAX_CONVERSATION_MEMBER_ROLE_BYTES,
            }
        );
    }

    #[test]
    fn conversation_member_roles_reject_control_characters_and_forged_wire_fields() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();

        assert_eq!(
            ConversationMember::new(conversation_id, person_id)
                .with_role(Some("moderator\0admin".to_owned()))
                .expect_err("NUL in a role must fail closed"),
            ConversationMemberValidationError::RoleContainsNul
        );
        assert_eq!(
            ConversationMember::new(conversation_id, person_id)
                .with_role(Some("x".repeat(MAX_CONVERSATION_MEMBER_ROLE_CHARS + 1)))
                .expect_err("role character count must be bounded"),
            ConversationMemberValidationError::RoleTooManyCharacters {
                length: MAX_CONVERSATION_MEMBER_ROLE_CHARS + 1,
                maximum: MAX_CONVERSATION_MEMBER_ROLE_CHARS,
            }
        );

        let member = ConversationMember::new(conversation_id, person_id);
        assert_eq!(member.role(), None);
        assert_eq!(member.validate(), Ok(()));
        let mut wire = serde_json::to_value(&member).expect("member should serialize");
        wire["unexpected"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<ConversationMember>(wire).is_err(),
            "portable member JSON must reject unknown fields"
        );
    }
}
