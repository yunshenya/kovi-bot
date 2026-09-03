//! World Model domain ids.
//!
//! Ids are distinct from Core's identity ids on purpose: an [`EntityId`] maps
//! onto a [`PersonId`] or [`ConversationId`] one-to-one, but the World Model
//! never copies the identity system.

world_id!(EntityId);
world_id!(ObservationId);
world_id!(SituationId);
world_id!(HypothesisId);
world_id!(PredictionId);
world_id!(CausalRelationId);
world_id!(UncertaintyId);
