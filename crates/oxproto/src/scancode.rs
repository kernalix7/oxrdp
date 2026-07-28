//! PS/2 set-1 scancodes (`docs/design/OXPROTO.md` §13).
// No module-level attributes are needed: the crate is already `#![forbid(unsafe_code)]`.

/// A PS/2 set-1 scancode plus whether it is an extended (E0-prefixed) code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scancode {
    /// The scancode byte (make code).
    pub code: u16,
    /// Whether the make code is prefixed with 0xE0.
    pub extended: bool,
}

/// Physical keys, named by their US-QWERTY position — the name is a label for the position,
/// not the character produced, because the guest applies its own layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    // Letters A–Z.
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    // Top-row digits, left to right (1, 2, … 9, 0).
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Digit0,
    // Control-punctuation block, in physical order.
    Escape,
    Minus,
    Equal,
    Backspace,
    Tab,
    BracketLeft,
    BracketRight,
    Enter,
    Semicolon,
    Quote,
    Backquote,
    Backslash,
    Comma,
    Period,
    Slash,
    Space,
    CapsLock,
    // Modifier keys.
    LeftShift,
    RightShift,
    LeftControl,
    LeftAlt,
    // Function keys.
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    // Lock keys.
    NumLock,
    ScrollLock,
    // Numpad keys (non-extended; NumLock on).
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadMultiply,
    NumpadSubtract,
    NumpadAdd,
    NumpadDecimal,
    // Extended (E0-prefixed) keys.
    RightControl,
    RightAlt,
    NumpadDivide,
    NumpadEnter,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    LeftMeta,
    RightMeta,
    Menu,
    PrintScreen,
}

/// Every key this table knows, in declaration order — handy for exhaustive tests and for
/// enumerating over the full set-1 surface.
pub const ALL: &[Key] = &[
    Key::A,
    Key::B,
    Key::C,
    Key::D,
    Key::E,
    Key::F,
    Key::G,
    Key::H,
    Key::I,
    Key::J,
    Key::K,
    Key::L,
    Key::M,
    Key::N,
    Key::O,
    Key::P,
    Key::Q,
    Key::R,
    Key::S,
    Key::T,
    Key::U,
    Key::V,
    Key::W,
    Key::X,
    Key::Y,
    Key::Z,
    Key::Digit1,
    Key::Digit2,
    Key::Digit3,
    Key::Digit4,
    Key::Digit5,
    Key::Digit6,
    Key::Digit7,
    Key::Digit8,
    Key::Digit9,
    Key::Digit0,
    Key::Escape,
    Key::Minus,
    Key::Equal,
    Key::Backspace,
    Key::Tab,
    Key::BracketLeft,
    Key::BracketRight,
    Key::Enter,
    Key::Semicolon,
    Key::Quote,
    Key::Backquote,
    Key::Backslash,
    Key::Comma,
    Key::Period,
    Key::Slash,
    Key::Space,
    Key::CapsLock,
    Key::LeftShift,
    Key::RightShift,
    Key::LeftControl,
    Key::LeftAlt,
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
    Key::NumLock,
    Key::ScrollLock,
    Key::Numpad0,
    Key::Numpad1,
    Key::Numpad2,
    Key::Numpad3,
    Key::Numpad4,
    Key::Numpad5,
    Key::Numpad6,
    Key::Numpad7,
    Key::Numpad8,
    Key::Numpad9,
    Key::NumpadMultiply,
    Key::NumpadSubtract,
    Key::NumpadAdd,
    Key::NumpadDecimal,
    Key::RightControl,
    Key::RightAlt,
    Key::NumpadDivide,
    Key::NumpadEnter,
    Key::Home,
    Key::End,
    Key::PageUp,
    Key::PageDown,
    Key::Insert,
    Key::Delete,
    Key::ArrowUp,
    Key::ArrowDown,
    Key::ArrowLeft,
    Key::ArrowRight,
    Key::LeftMeta,
    Key::RightMeta,
    Key::Menu,
    Key::PrintScreen,
];

impl Key {
    /// The set-1 scancode for this key.
    #[allow(clippy::too_many_lines)]
    pub fn scancode(self) -> Scancode {
        let (code, extended) = match self {
            // Letters.
            Key::A => (0x1E, false),
            Key::B => (0x30, false),
            Key::C => (0x2E, false),
            Key::D => (0x20, false),
            Key::E => (0x12, false),
            Key::F => (0x21, false),
            Key::G => (0x22, false),
            Key::H => (0x23, false),
            Key::I => (0x17, false),
            Key::J => (0x24, false),
            Key::K => (0x25, false),
            Key::L => (0x26, false),
            Key::M => (0x32, false),
            Key::N => (0x31, false),
            Key::O => (0x18, false),
            Key::P => (0x19, false),
            Key::Q => (0x10, false),
            Key::R => (0x13, false),
            Key::S => (0x1F, false),
            Key::T => (0x14, false),
            Key::U => (0x16, false),
            Key::V => (0x2F, false),
            Key::W => (0x11, false),
            Key::X => (0x2D, false),
            Key::Y => (0x15, false),
            Key::Z => (0x2C, false),
            // Top-row digits.
            Key::Digit1 => (0x02, false),
            Key::Digit2 => (0x03, false),
            Key::Digit3 => (0x04, false),
            Key::Digit4 => (0x05, false),
            Key::Digit5 => (0x06, false),
            Key::Digit6 => (0x07, false),
            Key::Digit7 => (0x08, false),
            Key::Digit8 => (0x09, false),
            Key::Digit9 => (0x0A, false),
            Key::Digit0 => (0x0B, false),
            // Control-punctuation block.
            Key::Escape => (0x01, false),
            Key::Minus => (0x0C, false),
            Key::Equal => (0x0D, false),
            Key::Backspace => (0x0E, false),
            Key::Tab => (0x0F, false),
            Key::BracketLeft => (0x1A, false),
            Key::BracketRight => (0x1B, false),
            Key::Enter => (0x1C, false),
            Key::Semicolon => (0x27, false),
            Key::Quote => (0x28, false),
            Key::Backquote => (0x29, false),
            Key::Backslash => (0x2B, false),
            Key::Comma => (0x33, false),
            Key::Period => (0x34, false),
            Key::Slash => (0x35, false),
            Key::Space => (0x39, false),
            Key::CapsLock => (0x3A, false),
            // Modifier keys.
            Key::LeftShift => (0x2A, false),
            Key::RightShift => (0x36, false),
            Key::LeftControl => (0x1D, false),
            Key::LeftAlt => (0x38, false),
            // Function keys.
            Key::F1 => (0x3B, false),
            Key::F2 => (0x3C, false),
            Key::F3 => (0x3D, false),
            Key::F4 => (0x3E, false),
            Key::F5 => (0x3F, false),
            Key::F6 => (0x40, false),
            Key::F7 => (0x41, false),
            Key::F8 => (0x42, false),
            Key::F9 => (0x43, false),
            Key::F10 => (0x44, false),
            Key::F11 => (0x57, false),
            Key::F12 => (0x58, false),
            // Lock keys.
            Key::NumLock => (0x45, false),
            Key::ScrollLock => (0x46, false),
            // Numpad (non-extended).
            Key::Numpad0 => (0x52, false),
            Key::Numpad1 => (0x4F, false),
            Key::Numpad2 => (0x50, false),
            Key::Numpad3 => (0x51, false),
            Key::Numpad4 => (0x4B, false),
            Key::Numpad5 => (0x4C, false),
            Key::Numpad6 => (0x4D, false),
            Key::Numpad7 => (0x47, false),
            Key::Numpad8 => (0x48, false),
            Key::Numpad9 => (0x49, false),
            Key::NumpadMultiply => (0x37, false),
            Key::NumpadSubtract => (0x4A, false),
            Key::NumpadAdd => (0x4E, false),
            Key::NumpadDecimal => (0x53, false),
            // Extended (E0-prefixed) keys.
            Key::RightControl => (0x1D, true),
            Key::RightAlt => (0x38, true),
            Key::NumpadDivide => (0x35, true),
            Key::NumpadEnter => (0x1C, true),
            Key::Home => (0x47, true),
            Key::End => (0x4F, true),
            Key::PageUp => (0x49, true),
            Key::PageDown => (0x51, true),
            Key::Insert => (0x52, true),
            Key::Delete => (0x53, true),
            Key::ArrowUp => (0x48, true),
            Key::ArrowDown => (0x50, true),
            Key::ArrowLeft => (0x4B, true),
            Key::ArrowRight => (0x4D, true),
            Key::LeftMeta => (0x5B, true),
            Key::RightMeta => (0x5C, true),
            Key::Menu => (0x5D, true),
            // PrintScreen make sequence is E0 12 E0 7C; collapsed to a single 0x7C + E0 code.
            Key::PrintScreen => (0x7C, true),
        };
        Scancode { code, extended }
    }

    /// The key a set-1 scancode denotes, if it is one this table knows.
    pub fn from_scancode(code: u16, extended: bool) -> Option<Key> {
        ALL.iter()
            .copied()
            .find(|&k| k.scancode().code == code && k.scancode().extended == extended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Equality helper spelling out both fields for readability in the anchor table.
    fn sc(code: u16, extended: bool) -> Scancode {
        Scancode { code, extended }
    }

    #[test]
    fn non_extended_anchors() {
        assert_eq!(Key::Escape.scancode(), sc(0x01, false));
        assert_eq!(Key::Digit1.scancode(), sc(0x02, false));
        assert_eq!(Key::Q.scancode(), sc(0x10, false));
        assert_eq!(Key::A.scancode(), sc(0x1E, false));
        assert_eq!(Key::Z.scancode(), sc(0x2C, false));
        assert_eq!(Key::Space.scancode(), sc(0x39, false));
        assert_eq!(Key::Enter.scancode(), sc(0x1C, false));
        assert_eq!(Key::LeftShift.scancode(), sc(0x2A, false));
        assert_eq!(Key::LeftControl.scancode(), sc(0x1D, false));
        assert_eq!(Key::LeftAlt.scancode(), sc(0x38, false));
        assert_eq!(Key::F1.scancode(), sc(0x3B, false));
        assert_eq!(Key::F12.scancode(), sc(0x58, false));
        assert_eq!(Key::NumLock.scancode(), sc(0x45, false));
        assert_eq!(Key::Numpad7.scancode(), sc(0x47, false));
    }

    #[test]
    fn extended_anchors() {
        assert_eq!(Key::RightControl.scancode(), sc(0x1D, true));
        assert_eq!(Key::RightAlt.scancode(), sc(0x38, true));
        assert_eq!(Key::ArrowUp.scancode(), sc(0x48, true));
        assert_eq!(Key::ArrowLeft.scancode(), sc(0x4B, true));
        assert_eq!(Key::ArrowRight.scancode(), sc(0x4D, true));
        assert_eq!(Key::ArrowDown.scancode(), sc(0x50, true));
        assert_eq!(Key::Home.scancode(), sc(0x47, true));
        assert_eq!(Key::End.scancode(), sc(0x4F, true));
        assert_eq!(Key::Insert.scancode(), sc(0x52, true));
        assert_eq!(Key::Delete.scancode(), sc(0x53, true));
        assert_eq!(Key::PageUp.scancode(), sc(0x49, true));
        assert_eq!(Key::PageDown.scancode(), sc(0x51, true));
        assert_eq!(Key::NumpadEnter.scancode(), sc(0x1C, true));
        assert_eq!(Key::NumpadDivide.scancode(), sc(0x35, true));
        assert_eq!(Key::LeftMeta.scancode(), sc(0x5B, true));
        assert_eq!(Key::RightMeta.scancode(), sc(0x5C, true));
        assert_eq!(Key::Menu.scancode(), sc(0x5D, true));
    }

    #[test]
    fn round_trip_exhaustive() {
        for &k in ALL {
            let s = k.scancode();
            assert_eq!(Key::from_scancode(s.code, s.extended), Some(k));
        }
    }

    #[test]
    fn unassigned_codes_are_none() {
        assert_eq!(Key::from_scancode(0x00, false), None);
        assert_eq!(Key::from_scancode(0xF0, false), None);
    }

    #[test]
    fn extended_disambiguates_same_byte() {
        assert_eq!(Key::from_scancode(0x1D, false), Some(Key::LeftControl));
        assert_eq!(Key::from_scancode(0x1D, true), Some(Key::RightControl));
        assert_ne!(
            Key::from_scancode(0x1D, false),
            Key::from_scancode(0x1D, true)
        );
    }
}
