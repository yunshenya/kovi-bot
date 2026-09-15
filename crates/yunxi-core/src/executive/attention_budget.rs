//! Logical attention/cognitive budgets, separate from model semaphores.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionCost {
    Ignore,
    ObserveOnly,
    Attend,
    DeepPlan,
    Reflect,
    DeepReflect,
}

impl AttentionCost {
    #[must_use]
    pub const fn units(self) -> f32 {
        match self {
            Self::Ignore => 0.0,
            Self::ObserveOnly => 0.1,
            Self::Attend => 1.0,
            Self::DeepPlan => 3.0,
            Self::Reflect => 4.0,
            Self::DeepReflect => 8.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AttentionBudget {
    pub total: f32,
    pub available: f32,
    pub reserved_for_critical: f32,
    pub replenishment_rate: f32,
}

impl Default for AttentionBudget {
    fn default() -> Self {
        Self {
            total: 20.0,
            available: 20.0,
            reserved_for_critical: 6.0,
            replenishment_rate: 1.0,
        }
    }
}

impl AttentionBudget {
    pub fn new(
        total: f32,
        reserved_for_critical: f32,
        replenishment_rate: f32,
    ) -> Result<Self, BudgetError> {
        let budget = Self {
            total,
            available: total,
            reserved_for_critical,
            replenishment_rate,
        };
        budget.validate()?;
        Ok(budget)
    }

    pub fn validate(self) -> Result<(), BudgetError> {
        if !self.total.is_finite()
            || !self.available.is_finite()
            || !self.reserved_for_critical.is_finite()
            || !self.replenishment_rate.is_finite()
            || self.total <= 0.0
            || self.available < 0.0
            || self.available > self.total
            || self.reserved_for_critical < 0.0
            || self.reserved_for_critical > self.total
            || self.replenishment_rate < 0.0
        {
            return Err(BudgetError::Invalid);
        }
        Ok(())
    }

    /// Rebuild a budget from its persisted, bounded representation.  Keeping
    /// this conversion in the domain module means hosts cannot accidentally
    /// restore unchecked floating point values into the live controller.
    pub fn from_snapshot(snapshot: AttentionBudgetSnapshot) -> Result<Self, BudgetError> {
        let budget = Self {
            total: snapshot.total,
            available: snapshot.available,
            reserved_for_critical: snapshot.reserved_for_critical,
            replenishment_rate: snapshot.replenishment_rate,
        };
        budget.validate()?;
        Ok(budget)
    }

    /// Critical work may consume the reserve. Ordinary work may not.
    pub fn try_consume(&mut self, cost: f32, critical: bool) -> Result<BudgetGrant, BudgetError> {
        if !cost.is_finite() || cost < 0.0 {
            return Err(BudgetError::InvalidCost);
        }
        if cost == 0.0 {
            return Ok(BudgetGrant::Granted {
                consumed: 0.0,
                remaining: self.available,
            });
        }
        let floor = if critical {
            0.0
        } else {
            self.reserved_for_critical
        };
        if self.available - cost < floor {
            return Ok(BudgetGrant::Denied {
                requested: cost,
                available: self.available,
                reserved: self.reserved_for_critical,
            });
        }
        self.available -= cost;
        Ok(BudgetGrant::Granted {
            consumed: cost,
            remaining: self.available,
        })
    }

    pub fn consume_kind(
        &mut self,
        cost: AttentionCost,
        critical: bool,
    ) -> Result<BudgetGrant, BudgetError> {
        self.try_consume(cost.units(), critical)
    }

    pub fn replenish(&mut self, elapsed: Duration) {
        let amount = self.replenishment_rate * elapsed.as_secs_f32();
        self.available = (self.available + amount).clamp(0.0, self.total);
    }

    pub fn reset(&mut self) {
        self.available = self.total;
    }

    #[must_use]
    pub fn snapshot(self) -> AttentionBudgetSnapshot {
        AttentionBudgetSnapshot {
            total: self.total,
            available: self.available,
            reserved_for_critical: self.reserved_for_critical,
            replenishment_rate: self.replenishment_rate,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AttentionBudgetSnapshot {
    pub total: f32,
    pub available: f32,
    pub reserved_for_critical: f32,
    pub replenishment_rate: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BudgetGrant {
    Granted {
        consumed: f32,
        remaining: f32,
    },
    Denied {
        requested: f32,
        available: f32,
        reserved: f32,
    },
}

impl BudgetGrant {
    #[must_use]
    pub const fn is_granted(self) -> bool {
        matches!(self, Self::Granted { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    Invalid,
    InvalidCost,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "attention budget is invalid",
            Self::InvalidCost => "attention cost is invalid",
        })
    }
}

impl std::error::Error for BudgetError {}

/// A separate integer budget makes it possible to account for model work
/// without conflating logical attention with physical semaphore capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitiveBudget {
    pub total: u32,
    pub available: u32,
    pub reserved_for_critical: u32,
    pub replenishment_per_minute: u32,
}

impl Default for CognitiveBudget {
    fn default() -> Self {
        Self {
            total: 20,
            available: 20,
            reserved_for_critical: 6,
            replenishment_per_minute: 1,
        }
    }
}

impl CognitiveBudget {
    pub fn try_consume(&mut self, units: u32, critical: bool) -> bool {
        let floor = if critical {
            0
        } else {
            self.reserved_for_critical
        };
        if self.available < units || self.available - units < floor {
            return false;
        }
        self.available -= units;
        true
    }

    pub fn replenish(&mut self, elapsed: Duration) {
        // 按分钟折算，但**不丢余数**：`as_secs() / 60` 会把不足一分钟的部分直接
        // 截掉，于是每秒调用一次的路径永远补不到量（每次都算出 0 分钟）。
        // 用 `as_secs_f64` 保留小数，再四舍五入成整数单位。
        let minutes = elapsed.as_secs_f64() / 60.0;
        let units = (f64::from(self.replenishment_per_minute) * minutes).round();
        if !units.is_finite() || units <= 0.0 {
            return;
        }
        let units = units.min(f64::from(u32::MAX)) as u32;
        self.available = self.available.saturating_add(units).min(self.total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_minute_replenishment_is_not_truncated_away() {
        // 原来按 `as_secs() / 60` 折算：每次调用不足一分钟的部分直接截掉，于是
        // "每 30 秒补一次"的路径永远算出 0 分钟、永远补不到量。
        let mut budget = CognitiveBudget {
            total: 20,
            available: 0,
            reserved_for_critical: 0,
            replenishment_per_minute: 2,
        };
        // 两次 30 秒 = 一分钟 → 应当补到 2 个单位。
        budget.replenish(Duration::from_secs(30));
        budget.replenish(Duration::from_secs(30));
        assert_eq!(budget.available, 2, "半分钟一次的补充不该被抹掉");
        // 单个 90 秒同样按 1.5 分钟折算（四舍五入到 3）。
        let mut other = CognitiveBudget {
            total: 20,
            available: 0,
            reserved_for_critical: 0,
            replenishment_per_minute: 2,
        };
        other.replenish(Duration::from_secs(90));
        assert_eq!(other.available, 3);
        // 上限与零时长不产生意外。
        other.replenish(Duration::ZERO);
        assert_eq!(other.available, 3);
        other.replenish(Duration::from_secs(600));
        assert_eq!(other.available, other.total);
    }
}
