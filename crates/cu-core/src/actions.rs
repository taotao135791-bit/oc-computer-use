//! Structured computer actions: the exact vocabulary the upper-layer model
//! speaks to the runtime. Every variant is validated on the way in (schema,
//! bounds, safety) and executed one-at-a-time by the runtime's action queue.

use serde::{Deserialize, Serialize};

use crate::coordinates::{CoordinateSpace, Point};

/// Mouse buttons understood by the macOS driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// CoreGraphics button number: 0 = left, 1 = right, 2 = middle/other.
    pub fn cg_button(self) -> u32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
        }
    }
}

impl std::str::FromStr for MouseButton {
    type Err = crate::errors::CuError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(MouseButton::Left),
            "right" => Ok(MouseButton::Right),
            "middle" => Ok(MouseButton::Middle),
            other => Err(crate::errors::CuError::InvalidParams(format!(
                "unknown mouse button `{other}`"
            ))),
        }
    }
}

/// How text is inserted. `keyboard` synthesizes key events with the unicode
/// string; `clipboard` swaps the pasteboard, pastes, and restores it (with a
/// fallback when synthetic key events are unreliable for CJK input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TextInputMethod {
    #[default]
    Keyboard,
    Clipboard,
}

impl std::str::FromStr for TextInputMethod {
    type Err = crate::errors::CuError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "keyboard" => Ok(TextInputMethod::Keyboard),
            "clipboard" => Ok(TextInputMethod::Clipboard),
            other => Err(crate::errors::CuError::InvalidParams(format!(
                "unknown text input method `{other}`"
            ))),
        }
    }
}

/// A single atomic computer action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComputerAction {
    Click {
        x: f64,
        y: f64,
        button: MouseButton,
        coordinate_space: CoordinateSpace,
    },
    DoubleClick {
        x: f64,
        y: f64,
        button: MouseButton,
        coordinate_space: CoordinateSpace,
    },
    Move {
        x: f64,
        y: f64,
        coordinate_space: CoordinateSpace,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    #[serde(rename = "type")]
    TypeText {
        text: String,
        #[serde(default)]
        method: TextInputMethod,
    },
    Key {
        keys: Vec<String>,
    },
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
        #[serde(default)]
        delta_x: f64,
        #[serde(default)]
        delta_y: f64,
        coordinate_space: CoordinateSpace,
    },
    Drag {
        from: Point,
        to: Point,
        coordinate_space: CoordinateSpace,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    Wait {
        duration_ms: u64,
    },
}

impl ComputerAction {
    pub fn type_name(&self) -> &'static str {
        match self {
            ComputerAction::Click { .. } => "click",
            ComputerAction::DoubleClick { .. } => "double_click",
            ComputerAction::Move { .. } => "move",
            ComputerAction::TypeText { .. } => "type",
            ComputerAction::Key { .. } => "key",
            ComputerAction::Scroll { .. } => "scroll",
            ComputerAction::Drag { .. } => "drag",
            ComputerAction::Wait { .. } => "wait",
        }
    }

    /// Coarse risk hint used by the policy layer and trace recording.
    /// Type/key/drag are treated as higher-touch than a bare pointer move.
    pub fn risk_level(&self) -> &'static str {
        match self {
            ComputerAction::Move { .. } | ComputerAction::Wait { .. } => "low",
            ComputerAction::Click { .. }
            | ComputerAction::DoubleClick { .. }
            | ComputerAction::Scroll { .. } => "medium",
            ComputerAction::TypeText { .. }
            | ComputerAction::Key { .. }
            | ComputerAction::Drag { .. } => "medium",
        }
    }
}

/// What the runtime should do after executing an action batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum WaitPolicy {
    #[default]
    None,
    Fixed,
    UntilStable,
}

impl std::str::FromStr for WaitPolicy {
    type Err = crate::errors::CuError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(WaitPolicy::None),
            "fixed" => Ok(WaitPolicy::Fixed),
            "until_stable" => Ok(WaitPolicy::UntilStable),
            other => Err(crate::errors::CuError::InvalidParams(format!(
                "unknown wait policy `{other}`"
            ))),
        }
    }
}

/// A batch of actions plus the policy governing its execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionBatch {
    pub actions: Vec<ComputerAction>,
    #[serde(default)]
    pub wait_policy: WaitPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_wait_ms: Option<u64>,
    #[serde(default)]
    pub return_screenshot: bool,
}

impl ActionBatch {
    /// Cap batch size and validate obvious shape problems before execution.
    pub fn validate(&self) -> Result<(), crate::errors::CuError> {
        use crate::errors::CuError;
        if self.actions.is_empty() {
            return Err(CuError::InvalidParams(
                "actions list must not be empty".into(),
            ));
        }
        if self.actions.len() > 64 {
            return Err(CuError::InvalidParams(
                "too many actions in one batch (max 64)".into(),
            ));
        }
        for a in &self.actions {
            if let ComputerAction::TypeText { text, .. } = a {
                if text.len() > 4096 {
                    return Err(CuError::InvalidParams(
                        "type text exceeds 4096 characters".into(),
                    ));
                }
                // NUL bytes and lone surrogates are never valid keyboard input.
                if text.contains('\0') {
                    return Err(CuError::InvalidParams("type text contains NUL byte".into()));
                }
            }
            if let ComputerAction::Key { keys } = a {
                if keys.is_empty() || keys.len() > 8 {
                    return Err(CuError::InvalidParams("key action needs 1..=8 keys".into()));
                }
            }
            if let ComputerAction::Wait { duration_ms } = a {
                if *duration_ms > 600_000 {
                    return Err(CuError::InvalidParams(
                        "wait duration exceeds 10 minutes".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Redacted description of a `type` action used in traces and logs by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedactedText {
    pub text_redacted: bool,
    pub character_count: usize,
}

impl RedactedText {
    pub fn from_text(text: &str) -> Self {
        Self {
            text_redacted: true,
            character_count: text.chars().count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::CuError;

    #[test]
    fn actions_serialize_with_type_tag() {
        let a = ComputerAction::Click {
            x: 100.0,
            y: 200.0,
            button: MouseButton::Left,
            coordinate_space: CoordinateSpace::Normalized1000,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["type"], "click");
        assert_eq!(v["x"], 100.0);
        assert_eq!(v["button"], "left");
        assert_eq!(v["coordinate_space"], "normalized_1000");
    }

    #[test]
    fn action_round_trip_all_variants() {
        let actions = vec![
            ComputerAction::Click {
                x: 1.0,
                y: 2.0,
                button: MouseButton::Right,
                coordinate_space: CoordinateSpace::ImagePixels,
            },
            ComputerAction::DoubleClick {
                x: 3.0,
                y: 4.0,
                button: MouseButton::Middle,
                coordinate_space: CoordinateSpace::Normalized1000,
            },
            ComputerAction::Move {
                x: 5.0,
                y: 6.0,
                coordinate_space: CoordinateSpace::Normalized1000,
                duration_ms: Some(120),
            },
            ComputerAction::TypeText {
                text: "你好".into(),
                method: TextInputMethod::Keyboard,
            },
            ComputerAction::Key {
                keys: vec!["CMD".into(), "L".into()],
            },
            ComputerAction::Scroll {
                x: None,
                y: None,
                delta_x: 0.0,
                delta_y: -300.0,
                coordinate_space: CoordinateSpace::Normalized1000,
            },
            ComputerAction::Drag {
                from: Point::new(0.0, 0.0),
                to: Point::new(10.0, 10.0),
                coordinate_space: CoordinateSpace::Normalized1000,
                duration_ms: None,
            },
            ComputerAction::Wait { duration_ms: 250 },
        ];
        for a in actions {
            let v = serde_json::to_value(&a).unwrap();
            let back: ComputerAction = serde_json::from_value(v).unwrap();
            assert_eq!(a, back, "round trip failed for {}", a.type_name());
        }
    }

    #[test]
    fn parse_uses_defaults_when_omitted() {
        let v = serde_json::json!({"type":"type","text":"hi"});
        let a: ComputerAction = serde_json::from_value(v).unwrap();
        assert_eq!(
            a,
            ComputerAction::TypeText {
                text: "hi".into(),
                method: TextInputMethod::Keyboard
            }
        );
    }

    #[test]
    fn batch_validate_rejects_empty_and_oversize() {
        let mut b = ActionBatch {
            actions: vec![],
            wait_policy: WaitPolicy::None,
            fixed_wait_ms: None,
            return_screenshot: false,
        };
        assert!(matches!(b.validate(), Err(CuError::InvalidParams(_))));
        b.actions = vec![ComputerAction::Wait {
            duration_ms: 700_000,
        }];
        assert!(matches!(b.validate(), Err(CuError::InvalidParams(_))));
    }

    #[test]
    fn batch_validate_rejects_nul() {
        let b = ActionBatch {
            actions: vec![ComputerAction::TypeText {
                text: "a\0b".into(),
                method: TextInputMethod::Keyboard,
            }],
            wait_policy: WaitPolicy::None,
            fixed_wait_ms: None,
            return_screenshot: false,
        };
        assert!(b.validate().is_err());
    }

    #[test]
    fn redacted_text_counts_chars() {
        let r = RedactedText::from_text("hello世界");
        assert_eq!(r.character_count, 7);
        assert!(r.text_redacted);
    }

    #[test]
    fn mouse_button_from_str() {
        assert_eq!("right".parse::<MouseButton>().unwrap(), MouseButton::Right);
        assert_eq!(MouseButton::Right.cg_button(), 1);
    }
}
