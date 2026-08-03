//! Keyboard synthesis: key combos (CMD+L, SHIFT+CMD+A, …) and unicode text
//! input via `CGEventKeyboardSetUnicodeString`. Virtual keycodes are the
//! stable Carbon values (kVK_ANSI_*) which do not change across macOS versions.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::ffi::*;

/// Map from a human-readable key name to its Carbon virtual keycode.
fn keycode_table() -> &'static HashMap<&'static str, u16> {
    static TABLE: OnceLock<HashMap<&'static str, u16>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        let letters = [
            ("A", 0x00),
            ("S", 0x01),
            ("D", 0x02),
            ("F", 0x03),
            ("H", 0x04),
            ("G", 0x05),
            ("Z", 0x06),
            ("X", 0x07),
            ("C", 0x08),
            ("V", 0x09),
            ("B", 0x0B),
            ("Q", 0x0C),
            ("W", 0x0D),
            ("E", 0x0E),
            ("R", 0x0F),
            ("Y", 0x10),
            ("T", 0x11),
            ("O", 0x1F),
            ("U", 0x20),
            ("I", 0x22),
            ("P", 0x23),
            ("L", 0x25),
            ("J", 0x26),
            ("K", 0x28),
            ("N", 0x2D),
            ("M", 0x2E),
        ];
        for (name, code) in letters {
            m.insert(name, code);
        }
        let digits = [
            ("0", 0x1D),
            ("1", 0x12),
            ("2", 0x13),
            ("3", 0x14),
            ("4", 0x15),
            ("5", 0x17),
            ("6", 0x16),
            ("7", 0x1A),
            ("8", 0x1C),
            ("9", 0x19),
        ];
        for (name, code) in digits {
            m.insert(name, code);
        }
        let symbols = [
            ("=", 0x18),
            ("-", 0x1B),
            ("]", 0x1E),
            ("[", 0x21),
            ("'", 0x27),
            (";", 0x29),
            ("\\", 0x2A),
            (",", 0x2B),
            ("/", 0x2C),
            (".", 0x2F),
            ("`", 0x32),
        ];
        for (name, code) in symbols {
            m.insert(name, code);
        }
        let special = [
            ("RETURN", 0x24),
            ("ENTER", 0x24),
            ("TAB", 0x30),
            ("SPACE", 0x31),
            ("DELETE", 0x33),
            ("BACKSPACE", 0x33),
            ("ESCAPE", 0x35),
            ("ESC", 0x35),
            ("CAPSLOCK", 0x39),
            ("FUNCTION", 0x3F),
            ("F1", 0x7A),
            ("F2", 0x78),
            ("F3", 0x63),
            ("F4", 0x76),
            ("F5", 0x60),
            ("F6", 0x61),
            ("F7", 0x62),
            ("F8", 0x64),
            ("F9", 0x65),
            ("F10", 0x6D),
            ("F11", 0x67),
            ("F12", 0x6F),
            ("F13", 0x69),
            ("F14", 0x6B),
            ("F15", 0x71),
            ("F16", 0x6A),
            ("F17", 0x40),
            ("F18", 0x4F),
            ("F19", 0x50),
            ("F20", 0x5A),
            ("HOME", 0x73),
            ("PAGEUP", 0x74),
            ("FORWARD_DELETE", 0x75),
            ("END", 0x77),
            ("PAGEDOWN", 0x79),
            ("LEFT", 0x7B),
            ("RIGHT", 0x7C),
            ("DOWN", 0x7D),
            ("UP", 0x7E),
            ("LEFT_ARROW", 0x7B),
            ("RIGHT_ARROW", 0x7C),
            ("DOWN_ARROW", 0x7D),
            ("UP_ARROW", 0x7E),
        ];
        for (name, code) in special {
            m.insert(name, code);
        }
        m
    })
}

/// Resolve a key name to a virtual keycode, if known.
pub fn virtual_keycode(name: &str) -> Option<u16> {
    let upper = name.trim().to_uppercase();
    if upper.len() == 1 {
        let c = upper.chars().next().unwrap();
        if c.is_ascii_alphanumeric() || "=-[]';\\,./`".contains(c) {
            return keycode_table().get(upper.as_str()).copied();
        }
    }
    keycode_table().get(upper.as_str()).copied()
}

/// Modifier flag bit for a recognized modifier name.
pub fn modifier_flag(name: &str) -> Option<u64> {
    match name.trim().to_uppercase().as_str() {
        "CMD" | "COMMAND" | "META" | "SUPER" => Some(FLAG_COMMAND),
        "CTRL" | "CONTROL" | "^" => Some(FLAG_CONTROL),
        "OPT" | "OPTION" | "ALT" => Some(FLAG_ALTERNATE),
        "SHIFT" => Some(FLAG_SHIFT),
        "FN" | "FUNCTION" => Some(FLAG_SECONDARY_FN),
        _ => None,
    }
}

/// A parsed key combo: modifier flags plus the final (action) keycode.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyCombo {
    pub flags: u64,
    pub keycode: Option<u16>,
}

/// Parse a list like `["SHIFT","CMD","L"]` into a [`KeyCombo`].
/// The last entry that is not a modifier is the action key; a combo made
/// entirely of modifiers is legal too (presses those modifiers).
pub fn parse_combo(keys: &[String]) -> Result<KeyCombo, cu_core::CuError> {
    let mut flags = 0u64;
    let mut keycode = None;
    for k in keys {
        if let Some(flag) = modifier_flag(k) {
            flags |= flag;
        } else if let Some(code) = virtual_keycode(k) {
            if keycode.is_some() {
                return Err(cu_core::CuError::InvalidParams(format!(
                    "key combo has more than one action key: {keys:?}"
                )));
            }
            keycode = Some(code);
        } else {
            return Err(cu_core::CuError::InvalidParams(format!(
                "unknown key `{k}` in combo {keys:?}"
            )));
        }
    }
    if keycode.is_none() && flags == 0 {
        return Err(cu_core::CuError::InvalidParams(format!(
            "empty key combo {keys:?}"
        )));
    }
    Ok(KeyCombo { flags, keycode })
}

/// Carbon virtual keycodes for modifier keys.
fn modifier_keycode(flags: u64) -> Option<u16> {
    match flags {
        f if f & FLAG_COMMAND != 0 => Some(0x37),   // kVK_Command
        f if f & FLAG_SHIFT != 0 => Some(0x38),     // kVK_Shift
        f if f & FLAG_ALTERNATE != 0 => Some(0x3A), // kVK_Option
        f if f & FLAG_CONTROL != 0 => Some(0x3B),   // kVK_Control
        f if f & FLAG_SECONDARY_FN != 0 => Some(0x3F), // kVK_Function
        _ => None,
    }
}

/// Post a key combo (e.g. CMD+L) by *pressing* the modifiers, tapping the key,
/// and releasing the modifiers.
///
/// Modifiers are posted as real key-down/key-up events, not just flag bits on
/// the action key: the macOS app switcher (CMD+TAB) and several system
/// shortcuts only react to combos whose modifier key was actually pressed.
pub fn post_combo(combo: &KeyCombo) {
    if combo.flags == 0 && combo.keycode.is_none() {
        return;
    }
    let mod_key = if combo.flags != 0 {
        modifier_keycode(combo.flags)
    } else {
        None
    };
    if let Some(mk) = mod_key {
        post(&create_keyboard_event(mk, true));
    }
    if let Some(code) = combo.keycode {
        let down = create_keyboard_event(code, true);
        set_flags(&down, combo.flags);
        post(&down);
        let up = create_keyboard_event(code, false);
        set_flags(&up, combo.flags);
        post(&up);
    }
    if let Some(mk) = mod_key {
        post(&create_keyboard_event(mk, false));
    }
}

/// Map one printable char to (keycode, needs_shift) on the US ANSI layout.
/// Returns `None` for characters with no US keycode (CJK, accented, emoji).
fn char_to_key_combo(c: char) -> Option<(u16, bool)> {
    if c.is_ascii_alphabetic() {
        // Physical (QWERTY) keycodes come from the table; uppercase holds shift.
        let lower = c.to_ascii_lowercase();
        let code = virtual_keycode(&lower.to_string())?;
        return Some((code, c.is_ascii_uppercase()));
    }
    match c {
        '0' => Some((0x1D, false)),
        '1' => Some((0x12, false)),
        '2' => Some((0x13, false)),
        '3' => Some((0x14, false)),
        '4' => Some((0x15, false)),
        '5' => Some((0x17, false)),
        '6' => Some((0x16, false)),
        '7' => Some((0x1A, false)),
        '8' => Some((0x1C, false)),
        '9' => Some((0x19, false)),
        ' ' => Some((0x31, false)),
        '\t' => Some((0x30, false)),
        '\n' => Some((0x24, false)),
        '`' => Some((0x32, false)),
        '~' => Some((0x32, true)),
        '!' => Some((0x12, true)),
        '@' => Some((0x13, true)),
        '#' => Some((0x14, true)),
        '$' => Some((0x15, true)),
        '%' => Some((0x17, true)),
        '^' => Some((0x16, true)),
        '&' => Some((0x1A, true)),
        '*' => Some((0x1C, true)),
        '(' => Some((0x19, true)),
        ')' => Some((0x1D, true)),
        '-' => Some((0x1B, false)),
        '_' => Some((0x1B, true)),
        '=' => Some((0x18, false)),
        '+' => Some((0x18, true)),
        '[' => Some((0x21, false)),
        '{' => Some((0x21, true)),
        ']' => Some((0x1E, false)),
        '}' => Some((0x1E, true)),
        '\\' => Some((0x2A, false)),
        '|' => Some((0x2A, true)),
        ';' => Some((0x29, false)),
        ':' => Some((0x29, true)),
        '\'' => Some((0x27, false)),
        '"' => Some((0x27, true)),
        ',' => Some((0x2B, false)),
        '<' => Some((0x2B, true)),
        '.' => Some((0x2F, false)),
        '>' => Some((0x2F, true)),
        '/' => Some((0x2C, false)),
        '?' => Some((0x2C, true)),
        _ => None,
    }
}

/// Insert text by typing *real keycodes* (shift held for uppercase/symbols).
///
/// Synthetic unicode-string key events are dropped by modern apps (notably
/// Terminal and Chromium), so every printable ASCII char is pressed as a
/// physical key. Characters without a US keycode (CJK, emoji, accented) fall
/// back to a unicode-string event; the clipboard input method remains the
/// reliable path for those and for IMEs.
pub fn type_text(text: &str) {
    for c in text.chars() {
        match char_to_key_combo(c) {
            Some((code, shift)) => {
                if shift {
                    post(&create_keyboard_event(0x38, true)); // kVK_Shift
                }
                let flags = if shift { FLAG_SHIFT } else { 0 };
                let down = create_keyboard_event(code, true);
                set_flags(&down, flags);
                post(&down);
                let up = create_keyboard_event(code, false);
                set_flags(&up, flags);
                post(&up);
                if shift {
                    post(&create_keyboard_event(0x38, false));
                }
            }
            None => {
                let down = create_keyboard_event(0, true);
                set_unicode(&down, &c.to_string());
                post(&down);
                let up = create_keyboard_event(0, false);
                set_unicode(&up, &c.to_string());
                post(&up);
            }
        }
    }
}

/// Post a single key tap with no modifiers (used for simple key actions).
pub fn tap_key(keycode: u16) {
    let down = create_keyboard_event(keycode, true);
    post(&down);
    let up = create_keyboard_event(keycode, false);
    post(&up);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_mapping_covers_us_layout() {
        assert_eq!(char_to_key_combo('a'), Some((0x00, false)));
        assert_eq!(char_to_key_combo('A'), Some((0x00, true)));
        assert_eq!(char_to_key_combo('z'), Some((0x06, false))); // kVK_ANSI_Z
        assert_eq!(char_to_key_combo('1'), Some((0x12, false)));
        assert_eq!(char_to_key_combo('!'), Some((0x12, true)));
        assert_eq!(char_to_key_combo(' '), Some((0x31, false)));
        assert_eq!(char_to_key_combo('@'), Some((0x13, true)));
        assert_eq!(char_to_key_combo('?'), Some((0x2C, true)));
        assert_eq!(char_to_key_combo('你'), None);
        assert_eq!(char_to_key_combo('🙂'), None);
    }

    #[test]
    fn letters_and_digits_resolve() {
        assert_eq!(virtual_keycode("a"), Some(0x00));
        assert_eq!(virtual_keycode("L"), Some(0x25));
        assert_eq!(virtual_keycode("1"), Some(0x12));
        assert_eq!(virtual_keycode("space"), Some(0x31));
    }

    #[test]
    fn unknown_keys_rejected() {
        assert_eq!(virtual_keycode("notakey"), None);
    }

    #[test]
    fn parse_combo_cmd_l() {
        let c = parse_combo(&["CMD".into(), "L".into()]).unwrap();
        assert_eq!(c.flags, FLAG_COMMAND);
        assert_eq!(c.keycode, Some(0x25));
    }

    #[test]
    fn parse_combo_shift_cmd_a() {
        let c = parse_combo(&["SHIFT".into(), "CMD".into(), "A".into()]).unwrap();
        assert_eq!(c.flags, FLAG_SHIFT | FLAG_COMMAND);
        assert_eq!(c.keycode, Some(0x00));
    }

    #[test]
    fn parse_combo_modifier_only() {
        let c = parse_combo(&["CMD".into()]).unwrap();
        assert_eq!(c.flags, FLAG_COMMAND);
        assert_eq!(c.keycode, None);
    }

    #[test]
    fn parse_combo_two_action_keys_rejected() {
        assert!(parse_combo(&["A".into(), "B".into()]).is_err());
    }

    #[test]
    fn parse_combo_unknown_rejected() {
        assert!(parse_combo(&["FROBNICATE".into()]).is_err());
    }

    #[test]
    fn modifier_names_map() {
        assert_eq!(modifier_flag("OPTION"), Some(FLAG_ALTERNATE));
        assert_eq!(modifier_flag("CTRL"), Some(FLAG_CONTROL));
        assert_eq!(modifier_flag("shift"), Some(FLAG_SHIFT));
        assert_eq!(modifier_flag("nope"), None);
    }
}
