//! The key table: `key` → (`code`, `keyCode`, inserted text).
//!
//! Data, deliberately in its own module and shaped as a table rather than a
//! match: it is transcribed from the UI Events code/key tables plus the legacy
//! `keyCode` values every hotkey library still reads, and a table can be
//! checked against them by eye.
//!
//! A driver names a key the way the spec does — `"a"`, `"A"`, `"Enter"`,
//! `"ArrowLeft"` — which is what CDP's `Input.dispatchKeyEvent` and WebDriver's
//! `sendKeys` both settle on.

/// One key's static data.
pub struct KeyDef {
    /// The `KeyboardEvent.key` value.
    pub key: &'static str,
    /// The `KeyboardEvent.code` value — the *physical* key, unaffected by
    /// modifiers, which is why `a` and `A` share `"KeyA"`.
    pub code: &'static str,
    /// The legacy `keyCode`/`which`.
    pub key_code: u32,
    /// The text this key inserts into a text control, if any. `None` for every
    /// non-printable key, which is exactly the test for "does this edit text".
    pub text: Option<&'static str>,
}

/// The named (non-printable) keys a driver can send. Printable characters are
/// resolved by [`lookup`] without a table entry.
static NAMED: &[KeyDef] = &[
    KeyDef {
        key: "Enter",
        code: "Enter",
        key_code: 13,
        text: None,
    },
    KeyDef {
        key: "Tab",
        code: "Tab",
        key_code: 9,
        text: None,
    },
    KeyDef {
        key: "Escape",
        code: "Escape",
        key_code: 27,
        text: None,
    },
    KeyDef {
        key: "Backspace",
        code: "Backspace",
        key_code: 8,
        text: None,
    },
    KeyDef {
        key: "Delete",
        code: "Delete",
        key_code: 46,
        text: None,
    },
    KeyDef {
        key: "ArrowLeft",
        code: "ArrowLeft",
        key_code: 37,
        text: None,
    },
    KeyDef {
        key: "ArrowUp",
        code: "ArrowUp",
        key_code: 38,
        text: None,
    },
    KeyDef {
        key: "ArrowRight",
        code: "ArrowRight",
        key_code: 39,
        text: None,
    },
    KeyDef {
        key: "ArrowDown",
        code: "ArrowDown",
        key_code: 40,
        text: None,
    },
    KeyDef {
        key: "Home",
        code: "Home",
        key_code: 36,
        text: None,
    },
    KeyDef {
        key: "End",
        code: "End",
        key_code: 35,
        text: None,
    },
    KeyDef {
        key: "PageUp",
        code: "PageUp",
        key_code: 33,
        text: None,
    },
    KeyDef {
        key: "PageDown",
        code: "PageDown",
        key_code: 34,
        text: None,
    },
    KeyDef {
        key: "Insert",
        code: "Insert",
        key_code: 45,
        text: None,
    },
    KeyDef {
        key: "Shift",
        code: "ShiftLeft",
        key_code: 16,
        text: None,
    },
    KeyDef {
        key: "Control",
        code: "ControlLeft",
        key_code: 17,
        text: None,
    },
    KeyDef {
        key: "Alt",
        code: "AltLeft",
        key_code: 18,
        text: None,
    },
    KeyDef {
        key: "Meta",
        code: "MetaLeft",
        key_code: 91,
        text: None,
    },
    KeyDef {
        key: "CapsLock",
        code: "CapsLock",
        key_code: 20,
        text: None,
    },
    KeyDef {
        key: "ContextMenu",
        code: "ContextMenu",
        key_code: 93,
        text: None,
    },
    KeyDef {
        key: "F1",
        code: "F1",
        key_code: 112,
        text: None,
    },
    KeyDef {
        key: "F2",
        code: "F2",
        key_code: 113,
        text: None,
    },
    KeyDef {
        key: "F3",
        code: "F3",
        key_code: 114,
        text: None,
    },
    KeyDef {
        key: "F4",
        code: "F4",
        key_code: 115,
        text: None,
    },
    KeyDef {
        key: "F5",
        code: "F5",
        key_code: 116,
        text: None,
    },
    KeyDef {
        key: "F6",
        code: "F6",
        key_code: 117,
        text: None,
    },
    KeyDef {
        key: "F7",
        code: "F7",
        key_code: 118,
        text: None,
    },
    KeyDef {
        key: "F8",
        code: "F8",
        key_code: 119,
        text: None,
    },
    KeyDef {
        key: "F9",
        code: "F9",
        key_code: 120,
        text: None,
    },
    KeyDef {
        key: "F10",
        code: "F10",
        key_code: 121,
        text: None,
    },
    KeyDef {
        key: "F11",
        code: "F11",
        key_code: 122,
        text: None,
    },
    KeyDef {
        key: "F12",
        code: "F12",
        key_code: 123,
        text: None,
    },
];

/// A resolved key: the same fields as [`KeyDef`] but owned, because printable
/// characters are computed rather than looked up.
pub struct ResolvedKey {
    pub key: String,
    pub code: String,
    pub key_code: u32,
    pub text: Option<String>,
}

/// Resolves a driver-supplied key name.
///
/// A single character is printable: it inserts itself, and its `code`/`keyCode`
/// come from the *unshifted* physical key (`A` and `a` are both `KeyA`/65),
/// which is what makes `shiftKey` meaningful rather than redundant.
#[must_use]
pub fn lookup(key: &str) -> ResolvedKey {
    if let Some(def) = NAMED.iter().find(|d| d.key == key) {
        return ResolvedKey {
            key: def.key.to_owned(),
            code: def.code.to_owned(),
            key_code: def.key_code,
            text: def.text.map(ToOwned::to_owned),
        };
    }

    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        let (code, key_code) = printable_code(ch);
        return ResolvedKey {
            key: key.to_owned(),
            code,
            key_code,
            text: Some(key.to_owned()),
        };
    }

    // An unknown multi-character name: honest empty `code`/`keyCode` rather
    // than a guess. The event still fires, so a listener sees the `key`.
    ResolvedKey {
        key: key.to_owned(),
        code: String::new(),
        key_code: 0,
        text: None,
    }
}

/// The `key` a physical `code` produces, the reverse of [`lookup`].
///
/// A driver may name only the physical key — CDP's `Input.dispatchKeyEvent`
/// allows `code` without `key` — and this is what turns that back into a `key`
/// the rest of the pipeline understands.
///
/// Deliberately the *same* two sources [`lookup`] uses, so the two cannot
/// drift: the [`NAMED`] table, then the codes [`printable_code`] emits. A code
/// neither of them knows (numpad, media keys) is `None` rather than a guess —
/// synthesizing a `code` the table would never produce is a lie about the
/// keyboard, and the caller can report a real error instead.
#[must_use]
pub fn key_for_code(code: &str, shift: bool) -> Option<String> {
    if let Some(def) = NAMED.iter().find(|d| d.code == code) {
        return Some(def.key.to_owned());
    }
    // The right-hand and keypad twins of keys [`NAMED`] stores once. They share
    // a `key` with their left/main counterpart — `KeyboardEvent.location` is
    // what tells them apart — so the reverse lookup has to know them even
    // though `lookup` will never *produce* them.
    let twin = match code {
        "ShiftRight" => Some("Shift"),
        "ControlRight" => Some("Control"),
        "AltRight" => Some("Alt"),
        "MetaRight" | "OSRight" => Some("Meta"),
        "NumpadEnter" => Some("Enter"),
        _ => None,
    };
    if let Some(key) = twin {
        return Some(key.to_owned());
    }
    if let Some(letter) = code.strip_prefix("Key")
        && letter.len() == 1
        && letter.as_bytes()[0].is_ascii_uppercase()
    {
        return Some(if shift {
            letter.to_owned()
        } else {
            letter.to_ascii_lowercase()
        });
    }
    if let Some(digit) = code.strip_prefix("Digit")
        && digit.len() == 1
        && let Ok(index) = digit.parse::<usize>()
    {
        // The US-layout shifted legends, in digit order starting at `0`.
        const SHIFTED_DIGITS: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];
        let ch = if shift {
            SHIFTED_DIGITS[index]
        } else {
            digit.as_bytes()[0] as char
        };
        return Some(ch.to_string());
    }
    let (unshifted, shifted) = match code {
        "Space" => (' ', ' '),
        "Backquote" => ('`', '~'),
        "Minus" => ('-', '_'),
        "Equal" => ('=', '+'),
        "BracketLeft" => ('[', '{'),
        "BracketRight" => (']', '}'),
        "Backslash" => ('\\', '|'),
        "Semicolon" => (';', ':'),
        "Quote" => ('\'', '"'),
        "Comma" => (',', '<'),
        "Period" => ('.', '>'),
        "Slash" => ('/', '?'),
        _ => return None,
    };
    Some(if shift { shifted } else { unshifted }.to_string())
}

/// `code` and legacy `keyCode` for a printable character, from the physical
/// US-layout key that produces it.
fn printable_code(ch: char) -> (String, u32) {
    let upper = ch.to_ascii_uppercase();
    match upper {
        'A'..='Z' => (format!("Key{upper}"), upper as u32),
        '0'..='9' => (format!("Digit{upper}"), upper as u32),
        ' ' => ("Space".to_owned(), 32),
        // The punctuation keys, by the physical key that carries them — both
        // the unshifted and shifted legend map to the same `code`.
        '`' | '~' => ("Backquote".to_owned(), 192),
        '-' | '_' => ("Minus".to_owned(), 189),
        '=' | '+' => ("Equal".to_owned(), 187),
        '[' | '{' => ("BracketLeft".to_owned(), 219),
        ']' | '}' => ("BracketRight".to_owned(), 221),
        '\\' | '|' => ("Backslash".to_owned(), 220),
        ';' | ':' => ("Semicolon".to_owned(), 186),
        '\'' | '"' => ("Quote".to_owned(), 222),
        ',' | '<' => ("Comma".to_owned(), 188),
        '.' | '>' => ("Period".to_owned(), 190),
        '/' | '?' => ("Slash".to_owned(), 191),
        '!' => ("Digit1".to_owned(), 49),
        '@' => ("Digit2".to_owned(), 50),
        '#' => ("Digit3".to_owned(), 51),
        '$' => ("Digit4".to_owned(), 52),
        '%' => ("Digit5".to_owned(), 53),
        '^' => ("Digit6".to_owned(), 54),
        '&' => ("Digit7".to_owned(), 55),
        '*' => ("Digit8".to_owned(), 56),
        '(' => ("Digit9".to_owned(), 57),
        ')' => ("Digit0".to_owned(), 48),
        _ => (String::new(), 0),
    }
}
