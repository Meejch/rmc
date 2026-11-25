//! Physical keys (winit `KeyCode`, position-based) to RDP scancodes (set 1) and the
//! extended flag. The server applies its own keyboard layout, so only the key position
//! matters here (German layout: `IntlBackslash` is the `<>` key).

use winit::keyboard::KeyCode;

/// `(scancode, extended)` or `None` for keys without an RDP scancode.
pub fn scancode(code: KeyCode) -> Option<(u8, bool)> {
    use KeyCode::*;
    let n = |c: u8| Some((c, false));
    let e = |c: u8| Some((c, true));
    match code {
        Escape => n(0x01),
        Digit1 => n(0x02),
        Digit2 => n(0x03),
        Digit3 => n(0x04),
        Digit4 => n(0x05),
        Digit5 => n(0x06),
        Digit6 => n(0x07),
        Digit7 => n(0x08),
        Digit8 => n(0x09),
        Digit9 => n(0x0A),
        Digit0 => n(0x0B),
        Minus => n(0x0C),
        Equal => n(0x0D),
        Backspace => n(0x0E),
        Tab => n(0x0F),
        KeyQ => n(0x10),
        KeyW => n(0x11),
        KeyE => n(0x12),
        KeyR => n(0x13),
        KeyT => n(0x14),
        KeyY => n(0x15),
        KeyU => n(0x16),
        KeyI => n(0x17),
        KeyO => n(0x18),
        KeyP => n(0x19),
        BracketLeft => n(0x1A),
        BracketRight => n(0x1B),
        Enter => n(0x1C),
        ControlLeft => n(0x1D),
        KeyA => n(0x1E),
        KeyS => n(0x1F),
        KeyD => n(0x20),
        KeyF => n(0x21),
        KeyG => n(0x22),
        KeyH => n(0x23),
        KeyJ => n(0x24),
        KeyK => n(0x25),
        KeyL => n(0x26),
        Semicolon => n(0x27),
        Quote => n(0x28),
        Backquote => n(0x29),
        ShiftLeft => n(0x2A),
        Backslash => n(0x2B),
        KeyZ => n(0x2C),
        KeyX => n(0x2D),
        KeyC => n(0x2E),
        KeyV => n(0x2F),
        KeyB => n(0x30),
        KeyN => n(0x31),
        KeyM => n(0x32),
        Comma => n(0x33),
        Period => n(0x34),
        Slash => n(0x35),
        ShiftRight => n(0x36),
        NumpadMultiply => n(0x37),
        AltLeft => n(0x38),
        Space => n(0x39),
        CapsLock => n(0x3A),
        F1 => n(0x3B),
        F2 => n(0x3C),
        F3 => n(0x3D),
        F4 => n(0x3E),
        F5 => n(0x3F),
        F6 => n(0x40),
        F7 => n(0x41),
        F8 => n(0x42),
        F9 => n(0x43),
        F10 => n(0x44),
        NumLock => n(0x45),
        ScrollLock => n(0x46),
        Numpad7 => n(0x47),
        Numpad8 => n(0x48),
        Numpad9 => n(0x49),
        NumpadSubtract => n(0x4A),
        Numpad4 => n(0x4B),
        Numpad5 => n(0x4C),
        Numpad6 => n(0x4D),
        NumpadAdd => n(0x4E),
        Numpad1 => n(0x4F),
        Numpad2 => n(0x50),
        Numpad3 => n(0x51),
        Numpad0 => n(0x52),
        NumpadDecimal => n(0x53),
        IntlBackslash => n(0x56),
        F11 => n(0x57),
        F12 => n(0x58),
        IntlRo => n(0x73),
        IntlYen => n(0x7D),
        NumpadEnter => e(0x1C),
        ControlRight => e(0x1D),
        NumpadDivide => e(0x35),
        PrintScreen => e(0x37),
        AltRight => e(0x38),
        Home => e(0x47),
        ArrowUp => e(0x48),
        PageUp => e(0x49),
        ArrowLeft => e(0x4B),
        ArrowRight => e(0x4D),
        End => e(0x4F),
        ArrowDown => e(0x50),
        PageDown => e(0x51),
        Insert => e(0x52),
        Delete => e(0x53),
        SuperLeft => e(0x5B),
        SuperRight => e(0x5C),
        ContextMenu => e(0x5D),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_extended_keys() {
        assert_eq!(scancode(KeyCode::KeyA), Some((0x1E, false)));
        assert_eq!(scancode(KeyCode::KeyZ), Some((0x2C, false)));
        assert_eq!(scancode(KeyCode::IntlBackslash), Some((0x56, false)));
        assert_eq!(scancode(KeyCode::ArrowRight), Some((0x4D, true)));
        assert_eq!(scancode(KeyCode::Numpad6), Some((0x4D, false)));
        assert_eq!(scancode(KeyCode::AltRight), Some((0x38, true)));
        assert_eq!(scancode(KeyCode::Fn), None);
    }

    #[test]
    fn no_duplicate_scancodes() {
        use std::collections::HashMap;
        let all = [
            KeyCode::Escape,
            KeyCode::Digit1,
            KeyCode::KeyQ,
            KeyCode::KeyA,
            KeyCode::KeyZ,
            KeyCode::Enter,
            KeyCode::NumpadEnter,
            KeyCode::ControlLeft,
            KeyCode::ControlRight,
            KeyCode::Home,
            KeyCode::Numpad7,
            KeyCode::Delete,
            KeyCode::NumpadDecimal,
            KeyCode::Slash,
            KeyCode::NumpadDivide,
            KeyCode::SuperLeft,
            KeyCode::F12,
        ];
        let mut seen = HashMap::new();
        for k in all {
            let sc = scancode(k).unwrap();
            assert!(seen.insert(sc, k).is_none(), "{k:?} collides");
        }
    }
}
