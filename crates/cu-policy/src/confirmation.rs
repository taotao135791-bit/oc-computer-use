//! Confirmation policy: the runtime can refuse an action batch when the
//! caller (or the batch itself) declares it high-risk and no confirmation has
//! been recorded. The runtime does *not* judge what is semantically dangerous —
//! the upper-layer agent owns that decision. It merely enforces the declared
//! policy so a misbehaving agent cannot silently act.

use cu_core::{errors::CuError, ComputerAction};

/// Risk levels understood by the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn parse(s: Option<&str>) -> RiskLevel {
        match s.unwrap_or("").to_lowercase().as_str() {
            "high" => RiskLevel::High,
            "medium" => RiskLevel::Medium,
            _ => RiskLevel::Low,
        }
    }
}

/// Declared confirmation requirement for a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmationPolicy {
    pub requires_confirmation: bool,
    pub risk_level: RiskLevel,
    pub reason: String,
    pub policy_context: Option<String>,
}

impl ConfirmationPolicy {
    /// Build the policy from the caller-supplied fields on `computer.act`.
    pub fn from_caller(
        requires_confirmation: Option<bool>,
        risk_level: Option<&str>,
        policy_context: Option<&str>,
        actions: &[ComputerAction],
    ) -> Self {
        let risk = RiskLevel::parse(risk_level);
        let explicit = requires_confirmation.unwrap_or(false);
        let auto_high = !explicit && risk == RiskLevel::High && !actions.is_empty();
        Self {
            requires_confirmation: explicit || auto_high,
            risk_level: risk,
            reason: if explicit {
                "caller marked the batch as requiring confirmation".to_string()
            } else if auto_high {
                "batch is declared high-risk".to_string()
            } else {
                String::new()
            },
            policy_context: policy_context.map(|s| s.to_string()),
        }
    }

    /// Error raised when the batch is not authorized yet.
    pub fn to_error(&self) -> CuError {
        CuError::ConfirmationRequired(cu_core::errors::ConfirmationDetail {
            reason: self.reason.clone(),
            risk_level: format!("{:?}", self.risk_level).to_lowercase(),
            requires_confirmation: self.requires_confirmation,
            policy_context: self.policy_context.clone(),
        })
    }
}

/// Whether a batch is authorized to run right now.
pub enum Authorization {
    Allowed,
    /// Reject now, and surface the structured reason via [`ConfirmationPolicy::to_error`].
    RequiresConfirmation(ConfirmationPolicy),
}

/// Decide authorization for a batch given a "confirmed" flag the harness may
/// hold (e.g. the user already approved via takeover/release flow).
pub fn authorize(
    policy: &ConfirmationPolicy,
    confirmed: bool,
) -> Authorization {
    if policy.requires_confirmation && !confirmed {
        Authorization::RequiresConfirmation(policy.clone())
    } else {
        Authorization::Allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::{ComputerAction, MouseButton};

    fn click() -> ComputerAction {
        ComputerAction::Click {
            x: 1.0,
            y: 2.0,
            button: MouseButton::Left,
            coordinate_space: cu_core::CoordinateSpace::Normalized1000,
        }
    }

    #[test]
    fn high_risk_auto_requires_confirmation() {
        let p = ConfirmationPolicy::from_caller(None, Some("high"), None, &[click()]);
        assert!(p.requires_confirmation);
        assert!(matches!(authorize(&p, false), Authorization::RequiresConfirmation(_)));
        assert!(matches!(authorize(&p, true), Authorization::Allowed));
    }

    #[test]
    fn low_risk_without_flag_allowed() {
        let p = ConfirmationPolicy::from_caller(None, Some("low"), None, &[click()]);
        assert!(!p.requires_confirmation);
        assert!(matches!(authorize(&p, false), Authorization::Allowed));
    }

    #[test]
    fn explicit_flag_wins_even_for_low_risk() {
        let p = ConfirmationPolicy::from_caller(Some(true), Some("low"), None, &[click()]);
        assert!(p.requires_confirmation);
        assert!(p.to_error().code() == cu_core::ErrorCode::ConfirmationRequired);
    }
}
