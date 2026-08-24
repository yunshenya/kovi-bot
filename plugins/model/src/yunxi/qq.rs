#![expect(
    dead_code,
    reason = "QQ canonical reference helpers are consumed by the Phase 2 shadow bridge"
)]

use thiserror::Error;
use yunxi_core::{
    ConversationKind, ExternalConversation, ExternalIdentity, ExternalReferenceError, PlatformId,
};

const QQ_PLATFORM: &str = "qq";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QqReferenceError {
    #[error("{field} must be a positive QQ identifier")]
    NonPositiveId { field: &'static str },
    #[error(transparent)]
    InvalidExternalReference(#[from] ExternalReferenceError),
}

pub fn person(user_id: i64) -> Result<ExternalIdentity, QqReferenceError> {
    positive(user_id, "user_id")?;
    Ok(ExternalIdentity::new(platform()?, user_id.to_string())?)
}

pub fn group(group_id: i64) -> Result<ExternalConversation, QqReferenceError> {
    positive(group_id, "group_id")?;
    Ok(ExternalConversation::new(
        platform()?,
        format!("group:{group_id}"),
        ConversationKind::Group,
    )?)
}

pub fn direct(self_id: i64, peer_user_id: i64) -> Result<ExternalConversation, QqReferenceError> {
    positive(self_id, "self_id")?;
    positive(peer_user_id, "peer_user_id")?;
    Ok(ExternalConversation::new(
        platform()?,
        format!("direct:{self_id}:{peer_user_id}"),
        ConversationKind::Direct,
    )?)
}

fn platform() -> Result<PlatformId, ExternalReferenceError> {
    PlatformId::new(QQ_PLATFORM)
}

fn positive(value: i64, field: &'static str) -> Result<(), QqReferenceError> {
    if value <= 0 {
        return Err(QqReferenceError::NonPositiveId { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{QqReferenceError, direct, group, person};
    use yunxi_core::ConversationKind;

    #[test]
    fn qq_ids_have_stable_canonical_external_keys() {
        let person = person(123).expect("positive user id");
        let group = group(456).expect("positive group id");
        let direct = direct(789, 123).expect("positive direct ids");

        assert_eq!(person.platform().as_str(), "qq");
        assert_eq!(person.external_id(), "123");
        assert_eq!(group.external_id(), "group:456");
        assert_eq!(group.kind(), ConversationKind::Group);
        assert_eq!(direct.external_id(), "direct:789:123");
        assert_eq!(direct.kind(), ConversationKind::Direct);
    }

    #[test]
    fn direct_keys_include_the_bot_and_cannot_collide_with_groups() {
        let first_bot = direct(10, 20).expect("valid direct key");
        let second_bot = direct(11, 20).expect("valid direct key");
        let group = group(20).expect("valid group key");

        assert_ne!(first_bot, second_bot);
        assert_ne!(first_bot.external_id(), group.external_id());
    }

    #[test]
    fn non_positive_qq_ids_are_rejected() {
        assert_eq!(
            person(0),
            Err(QqReferenceError::NonPositiveId { field: "user_id" })
        );
        assert_eq!(
            group(-1),
            Err(QqReferenceError::NonPositiveId { field: "group_id" })
        );
        assert_eq!(
            direct(0, 1),
            Err(QqReferenceError::NonPositiveId { field: "self_id" })
        );
        assert_eq!(
            direct(1, 0),
            Err(QqReferenceError::NonPositiveId {
                field: "peer_user_id"
            })
        );
    }
}
