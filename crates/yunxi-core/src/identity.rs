use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    Group,
    System,
}

#[cfg(test)]
mod tests {
    use super::{ConversationId, MessageId, PersonId};

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
}
