//! Temporal model: time is not a bare timestamp (v4 §34–38, §181).
//!
//! Times can be instants or ranges, carry a precision, and every dynamic
//! state can be classified Fresh / Stale / Expired / Unknown.

use super::{
    WorldValidationError,
    common::validate_unit,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Live freshness classification (v4 §90).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    Expired,
    Unknown,
}

/// Classify a state with `observed_at` and optional `expires_at` at `now`.
///
/// - now before observed → Unknown (clock skew / future observation);
/// - today is within the last 20% of the validity window → Stale;
/// - now past expires → Expired;
/// - never expires → Fresh (until observed, per the rule above).
#[must_use]
pub fn freshness_at(
    observed_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Freshness {
    if now < observed_at {
        return Freshness::Unknown;
    }
    let Some(expires_at) = expires_at else {
        return Freshness::Fresh;
    };
    if now > expires_at {
        return Freshness::Expired;
    }
    let window = expires_at - observed_at;
    let stale_at = expires_at - window / 5;
    if now >= stale_at {
        Freshness::Stale
    } else {
        Freshness::Fresh
    }
}

/// Where a relative ("明天下午") estimate is quantized (v4 §38, §224).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimePrecision {
    Exact,
    Approximate,
    Unknown,
}

/// A time point or range. "下午" becomes a range, not 15:00:00.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimeInterval {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    precision: TimePrecision,
    confidence: f32,
}

impl TimeInterval {
    pub fn new(
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        precision: TimePrecision,
        confidence: f32,
    ) -> Result<Self, WorldValidationError> {
        let confidence = validate_unit(confidence, "interval confidence")?;
        let interval = Self {
            start,
            end,
            precision,
            confidence,
        };
        interval.validate()?;
        Ok(interval)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence, "interval confidence")?;
        if let (Some(start), Some(end)) = (self.start, self.end)
            && end < start
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "time interval end precedes its start",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn start(&self) -> Option<DateTime<Utc>> {
        self.start
    }

    #[must_use]
    pub const fn end(&self) -> Option<DateTime<Utc>> {
        self.end
    }

    #[must_use]
    pub const fn precision(&self) -> TimePrecision {
        self.precision
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    pub fn is_instant(&self) -> bool {
        matches!((self.start, self.end), (Some(a), Some(b)) if a == b)
    }

    /// Does `now` fall inside the interval (both sides inclusive when set)?
    #[must_use]
    pub fn contains(&self, now: DateTime<Utc>) -> bool {
        self.start.is_none_or(|start| now >= start)
            && self.end.is_none_or(|end| now <= end)
    }

    /// Does this interval overlap `other`?
    #[must_use]
    pub fn overlaps(&self, other: &TimeInterval) -> bool {
        match (self.start, self.end, other.start, other.end) {
            (Some(a1), Some(a2), Some(b1), Some(b2)) => a1 <= b2 && b1 <= a2,
            // open intervals overlap everything except strictly-before/after.
            _ => true,
        }
    }

    /// Confirmed duration; None when an endpoint is open.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        match (self.start, self.end) {
            (Some(start), Some(end)) => Some(end - start),
            _ => None,
        }
    }
}

/// Relative temporal relation between two intervals (v4 §35).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalRelation {
    Before,
    After,
    During,
    Overlaps,
    Starts,
    Ends,
    ExpectedAt,
}

/// Deterministic relation between two intervals (v4 §35).
/// `ExpectedAt` is returned when `a` is a point falling inside `b`.
#[must_use]
pub fn relation_between(a: &TimeInterval, b: &TimeInterval) -> TemporalRelation {
    use TemporalRelation::*;
    match (a.start, a.end, b.start, b.end) {
        (Some(a_start), Some(a_end), Some(b_start), Some(b_end)) => {
            if a_start == a_end && b_start <= a_start && a_start <= b_end {
                ExpectedAt
            } else if a_end < b_start {
                Before
            } else if a_start > b_end {
                After
            } else if a_start <= b_start && b_end <= a_end {
                if a_start == b_start && a_end == b_end {
                    During
                } else if a_start == b_start {
                    Starts
                } else if a_end == b_end {
                    Ends
                } else {
                    During
                }
            } else if b_start <= a_start && a_end <= b_end {
                if a_start == b_start {
                    Starts
                } else {
                    During
                }
            } else {
                Overlaps
            }
        }
        (Some(point), None, Some(anchor), Some(anchor_end))
            if anchor <= point && point <= anchor_end =>
        {
            ExpectedAt
        }
        _ => Overlaps,
    }
}

/// What a timeline entry refers to (v4 §36).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorldRef {
    Entity(super::EntityId),
    Situation(super::SituationId),
}

/// One timeline entry (v4 §36).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEntry {
    subject: WorldRef,
    interval: TimeInterval,
    observed_at: DateTime<Utc>,
    version: u64,
}

impl TimelineEntry {
    pub fn new(
        subject: WorldRef,
        interval: TimeInterval,
        observed_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let entry = Self {
            subject,
            interval,
            observed_at,
            version: 1,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        self.interval.validate()?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        Ok(())
    }

    #[must_use]
    pub const fn subject(&self) -> WorldRef {
        self.subject
    }

    #[must_use]
    pub const fn interval(&self) -> &TimeInterval {
        &self.interval
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

/// Bounded timeline index (v4 §64).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TimelineState {
    entries: Vec<TimelineEntry>,
}

pub const MAX_TIMELINE_ENTRIES: usize = 256;

impl TimelineState {
    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.entries.len() > MAX_TIMELINE_ENTRIES {
            return Err(WorldValidationError::TooManyItems {
                field: "timeline entries",
                length: self.entries.len(),
                maximum: MAX_TIMELINE_ENTRIES,
            });
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Rebuild from persisted entries (validated, bounded).
    pub fn from_entries(entries: Vec<TimelineEntry>) -> Result<Self, WorldValidationError> {
        let state = Self { entries };
        state.validate()?;
        Ok(state)
    }

    pub fn iter(&self) -> impl Iterator<Item = &TimelineEntry> {
        self.entries.iter()
    }

    /// Push a new entry (dedupe: identical subject+interval replaces).
    pub fn push(&mut self, entry: TimelineEntry) -> Result<(), WorldValidationError> {
        entry.validate()?;
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|existing| existing.subject() == entry.subject()
                && existing.interval().start() == entry.interval().start()
                && existing.interval().end() == entry.interval().end())
        {
            *existing = entry;
        } else if self.entries.len() >= MAX_TIMELINE_ENTRIES {
            return Err(WorldValidationError::TooManyItems {
                field: "timeline entries",
                length: self.entries.len(),
                maximum: MAX_TIMELINE_ENTRIES,
            });
        } else {
            self.entries.push(entry);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn instant(value: DateTime<Utc>) -> TimeInterval {
        TimeInterval::new(Some(value), Some(value), TimePrecision::Exact, 1.0).expect("instant")
    }

    fn range(start: DateTime<Utc>, end: DateTime<Utc>) -> TimeInterval {
        TimeInterval::new(Some(start), Some(end), TimePrecision::Approximate, 0.8)
            .expect("range")
    }

    #[test]
    fn freshness_stages() {
        let now = Utc::now();
        assert_eq!(freshness_at(now, Some(now + Duration::minutes(60)), now), Freshness::Fresh);
        assert_eq!(
            freshness_at(now, Some(now + Duration::minutes(60)), now + Duration::minutes(40)),
            Freshness::Fresh
        );
        // Stale window = last 20% = 12 minutes.
        assert_eq!(
            freshness_at(now, Some(now + Duration::minutes(60)), now + Duration::minutes(50)),
            Freshness::Stale
        );
        assert_eq!(
            freshness_at(now, Some(now + Duration::minutes(60)), now + Duration::minutes(61)),
            Freshness::Expired
        );
        assert_eq!(freshness_at(now, Some(now + Duration::minutes(60)), now - Duration::minutes(1)), Freshness::Unknown);
        // Without an expiry, state stays fresh… until clock skew.
        assert_eq!(freshness_at(now, None, now + Duration::days(365)), Freshness::Fresh);
    }

    #[test]
    fn intervals_validate_and_support_ranges() {
        let now = Utc::now();
        let afternoon = range(now + Duration::hours(13), now + Duration::hours(18));
        assert!(afternoon.contains(now + Duration::hours(15)));
        assert!(!afternoon.contains(now + Duration::hours(12)));
        assert!(!afternoon.is_instant());
        assert!(instant(now).is_instant());

        assert!(TimeInterval::new(Some(now), Some(now - Duration::minutes(1)), TimePrecision::Exact, 1.0).is_err());
        assert!(TimeInterval::new(Some(now), None, TimePrecision::Approximate, 1.5).is_err());
    }

    #[test]
    fn relation_between_is_deterministic() {
        let now = Utc::now();
        let a = range(now, now + Duration::hours(1));
        let b = range(now + Duration::hours(2), now + Duration::hours(3));
        let c = range(now + Duration::minutes(30), now + Duration::minutes(40));
        let d = range(now, now + Duration::hours(1));
        let e = range(now, now + Duration::minutes(30));
        assert_eq!(relation_between(&a, &b), TemporalRelation::Before);
        assert_eq!(relation_between(&b, &a), TemporalRelation::After);
        assert_eq!(relation_between(&a, &c), TemporalRelation::During);
        assert_eq!(relation_between(&a, &d), TemporalRelation::During);
        assert_eq!(relation_between(&a, &e), TemporalRelation::Starts);
        assert_eq!(relation_between(&instant(now + Duration::hours(1)), &a), TemporalRelation::ExpectedAt);
    }

    #[test]
    fn timeline_dedupes_and_bounds() {
        let now = Utc::now();
        let subject = WorldRef::Entity(super::super::EntityId::new());
        let mut state = TimelineState::default();
        state
            .push(TimelineEntry::new(subject, range(now, now + Duration::hours(1)), now).expect("entry"))
            .expect("pushed");
        state
            .push(TimelineEntry::new(subject, range(now, now + Duration::hours(1)), now).expect("entry"))
            .expect("deduped");
        assert_eq!(state.len(), 1);
        state
            .push(
                TimelineEntry::new(
                    WorldRef::Situation(super::super::SituationId::new()),
                    range(now, now + Duration::hours(2)),
                    now,
                )
                .expect("entry"),
            )
            .expect("pushed");
        assert_eq!(state.len(), 2);
        state.validate().expect("valid");
    }
}
