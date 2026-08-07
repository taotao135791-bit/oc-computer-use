//! Session state machine shared by the runtime and the daemon.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle state of a computer-use session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Active,
    Paused,
    UserTakeover,
    Stopping,
    Stopped,
    Failed,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Starting => "starting",
            SessionState::Active => "active",
            SessionState::Paused => "paused",
            SessionState::UserTakeover => "user_takeover",
            SessionState::Stopping => "stopping",
            SessionState::Stopped => "stopped",
            SessionState::Failed => "failed",
        }
    }
}

/// Public, serializable view of a session's state, returned by
/// `computer.session` (action `status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub state: SessionState,
    pub paused: bool,
    pub user_takeover: bool,
    pub lock_held: bool,
    pub display_id: String,
    pub created_at: DateTime<Utc>,
    pub last_action_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_dir: Option<String>,
    pub started_by: String,
}

/// Optional app/window target for a session. When set, `computer_observe` is
/// scoped to the target window and `computer_act` rejects coordinates outside
/// its bounds. The runtime never auto-clicks other apps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct SessionTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<i64>,
}

/// How strictly keyboard focus is validated before `type`/`key`/`shortcut`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum FocusPolicy {
    /// Never steal foreground/key focus. If focus is not on the target,
    /// reject with `INPUT_FOCUS_MISMATCH`.
    #[default]
    Strict,
    /// Activate the target app/window before keyboard input (explicit opt-in).
    ActivateTarget,
}

/// Actions a caller may issue against a session via `computer.session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Start,
    Status,
    Pause,
    Resume,
    Stop,
    Takeover,
    Release,
}

impl std::str::FromStr for SessionAction {
    type Err = crate::errors::CuError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "start" => Ok(SessionAction::Start),
            "status" => Ok(SessionAction::Status),
            "pause" => Ok(SessionAction::Pause),
            "resume" => Ok(SessionAction::Resume),
            "stop" => Ok(SessionAction::Stop),
            "takeover" => Ok(SessionAction::Takeover),
            "release" => Ok(SessionAction::Release),
            other => Err(crate::errors::CuError::InvalidParams(format!(
                "unknown session action `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_serializes_to_snake() {
        let v = serde_json::to_value(SessionState::UserTakeover).unwrap();
        assert_eq!(v, "user_takeover");
        assert_eq!(SessionState::Active.as_str(), "active");
    }

    #[test]
    fn session_action_from_str() {
        assert_eq!(
            "takeover".parse::<SessionAction>().unwrap(),
            SessionAction::Takeover
        );
        assert!("bogus".parse::<SessionAction>().is_err());
    }
}
