//! Keyboard and pointer input as QMP `input-send-event` events.
//!
//! Keys are QEMU qcodes ([`QCODES`], from QEMU's `QKeyCode`). Text is typed on a US layout,
//! which is inputd's default keymap: [`char_keys`] gives the key for a character and
//! whether Shift is held. A key combination such as `ctrl-alt-f2` or `meta_l-f1` is a list
//! of keys pressed in order and released in reverse ([`parse_combo`]); common names
//! (`enter`, `super`, `space`, single characters) are accepted as aliases.
//!
//! On q35 the keyboard is the i8042 PS/2 keyboard (ps2d in the guest; the firmware's own
//! driver in the bootloader). The pointer is QEMU's vmmouse, which ps2d switches to
//! absolute mode: [`PointerEvent::Abs`] positions (`0..=0x7fff` on each axis) and button
//! events reach it, while relative motion goes to the PS/2 mouse, which ps2d ignores in
//! that mode. With `--input usb` a USB tablet and keyboard take the same events through
//! xhcid and usbhidd.
//!
//! The PS/2 controller holds only a few bytes, so keys are sent one event at a time with a
//! short delay ([`crate::machine::LaunchConfig::key_delay_ms`]) rather than as one batch.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

/// Every QEMU `QKeyCode` name.
pub const QCODES: &[&str] = &[
    "unmapped",
    "shift",
    "shift_r",
    "alt",
    "alt_r",
    "ctrl",
    "ctrl_r",
    "menu",
    "esc",
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    "0",
    "minus",
    "equal",
    "backspace",
    "tab",
    "q",
    "w",
    "e",
    "r",
    "t",
    "y",
    "u",
    "i",
    "o",
    "p",
    "bracket_left",
    "bracket_right",
    "ret",
    "a",
    "s",
    "d",
    "f",
    "g",
    "h",
    "j",
    "k",
    "l",
    "semicolon",
    "apostrophe",
    "grave_accent",
    "backslash",
    "z",
    "x",
    "c",
    "v",
    "b",
    "n",
    "m",
    "comma",
    "dot",
    "slash",
    "asterisk",
    "spc",
    "caps_lock",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "num_lock",
    "scroll_lock",
    "kp_divide",
    "kp_multiply",
    "kp_subtract",
    "kp_add",
    "kp_enter",
    "kp_decimal",
    "sysrq",
    "kp_0",
    "kp_1",
    "kp_2",
    "kp_3",
    "kp_4",
    "kp_5",
    "kp_6",
    "kp_7",
    "kp_8",
    "kp_9",
    "less",
    "f11",
    "f12",
    "print",
    "home",
    "pgup",
    "pgdn",
    "end",
    "left",
    "up",
    "down",
    "right",
    "insert",
    "delete",
    "stop",
    "again",
    "props",
    "undo",
    "front",
    "copy",
    "open",
    "paste",
    "find",
    "cut",
    "lf",
    "help",
    "meta_l",
    "meta_r",
    "compose",
    "pause",
    "ro",
    "hiragana",
    "henkan",
    "yen",
    "muhenkan",
    "katakanahiragana",
    "kp_comma",
    "kp_equals",
    "power",
    "sleep",
    "wake",
    "audionext",
    "audioprev",
    "audiostop",
    "audioplay",
    "audiomute",
    "volumeup",
    "volumedown",
    "mediaselect",
    "mail",
    "calculator",
    "computer",
    "ac_home",
    "ac_back",
    "ac_forward",
    "ac_refresh",
    "ac_bookmarks",
    "lang1",
    "lang2",
    "f13",
    "f14",
    "f15",
    "f16",
    "f17",
    "f18",
    "f19",
    "f20",
    "f21",
    "f22",
    "f23",
    "f24",
];

/// Input the harness cannot express.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InputError {
    /// A character with no key on a US layout.
    #[error("no US-layout key types {0:?}")]
    Char(char),
    /// A key name that is not a qcode or alias.
    #[error("unknown key {0:?} (QEMU qcode names, or an alias such as enter, super, space)")]
    Key(String),
    /// An empty combination.
    #[error("empty key combination")]
    EmptyCombo,
}

/// A validated QEMU qcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Qcode(&'static str);

impl Qcode {
    /// The qcode named `name`, if QEMU has one.
    pub fn new(name: &str) -> Option<Self> {
        QCODES.iter().find(|&&q| q == name).map(|&q| Self(q))
    }

    /// The qcode's name.
    pub const fn name(self) -> &'static str {
        self.0
    }

    /// The Shift key.
    pub const SHIFT: Qcode = Qcode("shift");
    /// Enter.
    pub const RET: Qcode = Qcode("ret");
}

/// The key that types `c` on a US layout, and whether Shift must be held.
///
/// # Errors
///
/// [`InputError::Char`] for characters a US keyboard has no key for.
pub fn char_keys(c: char) -> Result<(Qcode, bool), InputError> {
    let unshifted = |name: &str| Qcode::new(name).map(|q| (q, false));
    let shifted = |name: &str| Qcode::new(name).map(|q| (q, true));
    let found = match c {
        'a'..='z' | '0'..='9' => unshifted(&c.to_string()),
        'A'..='Z' => shifted(&c.to_ascii_lowercase().to_string()),
        ' ' => unshifted("spc"),
        '\n' | '\r' => unshifted("ret"),
        '\t' => unshifted("tab"),
        '\u{8}' => unshifted("backspace"),
        '\u{1b}' => unshifted("esc"),
        '-' => unshifted("minus"),
        '=' => unshifted("equal"),
        '[' => unshifted("bracket_left"),
        ']' => unshifted("bracket_right"),
        '\\' => unshifted("backslash"),
        ';' => unshifted("semicolon"),
        '\'' => unshifted("apostrophe"),
        '`' => unshifted("grave_accent"),
        ',' => unshifted("comma"),
        '.' => unshifted("dot"),
        '/' => unshifted("slash"),
        '!' => shifted("1"),
        '@' => shifted("2"),
        '#' => shifted("3"),
        '$' => shifted("4"),
        '%' => shifted("5"),
        '^' => shifted("6"),
        '&' => shifted("7"),
        '*' => shifted("8"),
        '(' => shifted("9"),
        ')' => shifted("0"),
        '_' => shifted("minus"),
        '+' => shifted("equal"),
        '{' => shifted("bracket_left"),
        '}' => shifted("bracket_right"),
        '|' => shifted("backslash"),
        ':' => shifted("semicolon"),
        '"' => shifted("apostrophe"),
        '~' => shifted("grave_accent"),
        '<' => shifted("comma"),
        '>' => shifted("dot"),
        '?' => shifted("slash"),
        _ => None,
    };
    found.ok_or(InputError::Char(c))
}

/// A key name or alias as a qcode.
///
/// # Errors
///
/// [`InputError::Key`] for names that are neither.
pub fn key_name(name: &str) -> Result<Qcode, InputError> {
    let lower = name.to_ascii_lowercase();
    let alias = match lower.as_str() {
        "enter" | "return" => Some("ret"),
        "space" => Some("spc"),
        "escape" => Some("esc"),
        "control" | "ctrl_l" => Some("ctrl"),
        "alt_l" => Some("alt"),
        "shift_l" => Some("shift"),
        "super" | "win" | "meta" | "super_l" => Some("meta_l"),
        "super_r" => Some("meta_r"),
        "del" => Some("delete"),
        "ins" => Some("insert"),
        "pageup" | "page_up" => Some("pgup"),
        "pagedown" | "page_down" => Some("pgdn"),
        "bksp" => Some("backspace"),
        _ => None,
    };
    if let Some(q) = Qcode::new(alias.unwrap_or(&lower)) {
        return Ok(q);
    }
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && let Ok((q, false)) = char_keys(c)
    {
        return Ok(q);
    }
    Err(InputError::Key(name.to_owned()))
}

/// Parses a combination: key names joined by `-` (`ctrl-alt-f2`, `meta_l-f1`, `ret`).
/// A lone `-` is the minus key.
///
/// # Errors
///
/// [`InputError::EmptyCombo`] or [`InputError::Key`].
pub fn parse_combo(combo: &str) -> Result<Vec<Qcode>, InputError> {
    if combo == "-" {
        return Ok(vec![Qcode("minus")]);
    }
    if combo.is_empty() {
        return Err(InputError::EmptyCombo);
    }
    combo.split('-').map(key_name).collect()
}

/// One keyboard or pointer event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// A key goes down or up.
    Key {
        /// The key.
        key: Qcode,
        /// Pressed (`true`) or released.
        down: bool,
    },
    /// A pointer event.
    Pointer(PointerEvent),
}

/// A pointer event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEvent {
    /// Absolute position on both axes, `0..=0x7fff`.
    Abs {
        /// Horizontal.
        x: u32,
        /// Vertical.
        y: u32,
    },
    /// Relative motion.
    Rel {
        /// Horizontal.
        dx: i32,
        /// Vertical.
        dy: i32,
    },
    /// A button goes down or up.
    Button {
        /// The button.
        button: Button,
        /// Pressed (`true`) or released.
        down: bool,
    },
}

/// A pointer button (QEMU's `InputButton`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Button {
    /// Left.
    Left,
    /// Right.
    Right,
    /// Middle.
    Middle,
    /// Scroll up.
    WheelUp,
    /// Scroll down.
    WheelDown,
}

impl Button {
    const fn qmp_name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
            Button::WheelUp => "wheel-up",
            Button::WheelDown => "wheel-down",
        }
    }
}

/// What a pointer button does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ButtonAction {
    /// Press and release.
    #[default]
    Click,
    /// Press.
    Down,
    /// Release.
    Up,
}

/// A pointer request from a client or scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PointerAction {
    /// Move to a position: screen pixels, or with `raw` QEMU's absolute axis units
    /// (`0..=0x7fff`).
    Abs {
        /// Horizontal.
        x: u32,
        /// Vertical.
        y: u32,
        /// `x` and `y` are axis units, not pixels.
        raw: bool,
    },
    /// Move by a relative amount.
    Rel {
        /// Horizontal.
        dx: i32,
        /// Vertical.
        dy: i32,
    },
    /// Press, release or click a button.
    Button {
        /// The button.
        button: Button,
        /// What to do.
        action: ButtonAction,
    },
}

/// Largest absolute axis value QEMU accepts.
pub const ABS_MAX: u32 = 0x7fff;

/// The absolute axis value that points at the centre of pixel `p` on a screen `size`
/// pixels wide (or high).
pub fn pixel_to_abs(p: u32, size: u32) -> u32 {
    if size == 0 {
        return 0;
    }
    let value = (u64::from(p) * 2 + 1) * u64::from(ABS_MAX + 1) / (u64::from(size) * 2);
    u32::try_from(value).unwrap_or(ABS_MAX).min(ABS_MAX)
}

impl InputEvent {
    /// The events of one `input-send-event` call (axes of one position go together).
    pub fn to_qmp(self) -> Vec<Value> {
        match self {
            InputEvent::Key { key, down } => vec![json!({
                "type": "key",
                "data": {"down": down, "key": {"type": "qcode", "data": key.name()}},
            })],
            InputEvent::Pointer(PointerEvent::Abs { x, y }) => vec![
                json!({"type": "abs", "data": {"axis": "x", "value": x.min(ABS_MAX)}}),
                json!({"type": "abs", "data": {"axis": "y", "value": y.min(ABS_MAX)}}),
            ],
            InputEvent::Pointer(PointerEvent::Rel { dx, dy }) => vec![
                json!({"type": "rel", "data": {"axis": "x", "value": dx}}),
                json!({"type": "rel", "data": {"axis": "y", "value": dy}}),
            ],
            InputEvent::Pointer(PointerEvent::Button { button, down }) => vec![json!({
                "type": "btn",
                "data": {"down": down, "button": button.qmp_name()},
            })],
        }
    }
}

/// The key events that type `text`.
///
/// # Errors
///
/// [`InputError::Char`] for the first character without a key.
pub fn type_events(text: &str) -> Result<Vec<InputEvent>, InputError> {
    let mut events = Vec::new();
    for c in text.chars() {
        let (key, shift) = char_keys(c)?;
        if shift {
            events.push(InputEvent::Key {
                key: Qcode::SHIFT,
                down: true,
            });
        }
        events.push(InputEvent::Key { key, down: true });
        events.push(InputEvent::Key { key, down: false });
        if shift {
            events.push(InputEvent::Key {
                key: Qcode::SHIFT,
                down: false,
            });
        }
    }
    Ok(events)
}

/// The key events of a combination: every key down in order, then up in reverse.
pub fn combo_events(keys: &[Qcode]) -> Vec<InputEvent> {
    keys.iter()
        .map(|&key| InputEvent::Key { key, down: true })
        .chain(
            keys.iter()
                .rev()
                .map(|&key| InputEvent::Key { key, down: false }),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(events: &[InputEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                InputEvent::Key { key, down } => {
                    format!("{}{}", if *down { "+" } else { "-" }, key.name())
                }
                InputEvent::Pointer(p) => format!("{p:?}"),
            })
            .collect()
    }

    #[test]
    fn every_printable_ascii_character_has_a_key() {
        for c in ' '..='~' {
            let (key, _) = char_keys(c).unwrap_or_else(|e| panic!("{e}"));
            assert!(QCODES.contains(&key.name()));
        }
        assert_eq!(char_keys('é'), Err(InputError::Char('é')));
    }

    #[test]
    fn typing_uses_shift_for_upper_case_and_symbols() {
        let events = type_events("Ab=_\n").expect("typeable");
        assert_eq!(
            names(&events),
            [
                "+shift", "+a", "-a", "-shift", "+b", "-b", "+equal", "-equal", "+shift", "+minus",
                "-minus", "-shift", "+ret", "-ret"
            ]
        );
    }

    #[test]
    fn combos_and_aliases() {
        let keys = parse_combo("ctrl-alt-f2").expect("combo");
        assert_eq!(
            names(&combo_events(&keys)),
            ["+ctrl", "+alt", "+f2", "-f2", "-alt", "-ctrl"]
        );
        assert_eq!(
            parse_combo("super-F1").expect("combo"),
            [Qcode("meta_l"), Qcode("f1")]
        );
        assert_eq!(parse_combo("enter").expect("combo"), [Qcode::RET]);
        assert_eq!(parse_combo("-").expect("combo"), [Qcode("minus")]);
        assert_eq!(
            parse_combo("ctrl-c").expect("combo"),
            [Qcode("ctrl"), Qcode("c")]
        );
        assert_eq!(parse_combo("/").expect("combo"), [Qcode("slash")]);
        assert_eq!(parse_combo("hyper-x"), Err(InputError::Key("hyper".into())));
        assert_eq!(parse_combo(""), Err(InputError::EmptyCombo));
    }

    #[test]
    fn qmp_encoding() {
        let key = InputEvent::Key {
            key: Qcode::RET,
            down: true,
        };
        assert_eq!(
            key.to_qmp(),
            [
                json!({"type": "key", "data": {"down": true, "key": {"type": "qcode", "data": "ret"}}})
            ]
        );
        let abs = InputEvent::Pointer(PointerEvent::Abs { x: 0x9000, y: 5 });
        assert_eq!(
            abs.to_qmp(),
            [
                json!({"type": "abs", "data": {"axis": "x", "value": 0x7fff}}),
                json!({"type": "abs", "data": {"axis": "y", "value": 5}}),
            ]
        );
        let btn = InputEvent::Pointer(PointerEvent::Button {
            button: Button::WheelUp,
            down: false,
        });
        assert_eq!(
            btn.to_qmp(),
            [json!({"type": "btn", "data": {"down": false, "button": "wheel-up"}})]
        );
        let rel = InputEvent::Pointer(PointerEvent::Rel { dx: -3, dy: 4 });
        assert_eq!(rel.to_qmp()[0]["data"]["value"], -3);
    }

    #[test]
    fn pixel_scaling() {
        assert_eq!(pixel_to_abs(0, 1280), 12);
        assert_eq!(pixel_to_abs(640, 1280), 16396);
        assert_eq!(pixel_to_abs(1279, 1280), 32755);
        assert_eq!(pixel_to_abs(5000, 1280), ABS_MAX);
        assert_eq!(pixel_to_abs(3, 0), 0);
    }
}
