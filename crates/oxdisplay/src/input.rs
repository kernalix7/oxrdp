use oxproto::message::input::{key_flag, lock_state, modifier, pointer_button};
use winit::event::{ElementState, MouseButton};
use winit::keyboard::{KeyCode, ModifiersState};

/// PS/2 set 1 scancode mapping result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scancode {
    /// Set 1 scancode without E0 prefix.
    pub code: u16,
    /// Whether the key uses the E0 prefix.
    pub extended: bool,
}

/// Translate a winit physical key code to PS/2 set 1.
#[must_use]
pub fn keycode_to_scancode(code: KeyCode) -> Option<Scancode> {
    use KeyCode::*;
    let scancode = match code {
        Escape => Scancode::plain(0x01),
        Digit1 => Scancode::plain(0x02),
        Digit2 => Scancode::plain(0x03),
        Digit3 => Scancode::plain(0x04),
        Digit4 => Scancode::plain(0x05),
        Digit5 => Scancode::plain(0x06),
        Digit6 => Scancode::plain(0x07),
        Digit7 => Scancode::plain(0x08),
        Digit8 => Scancode::plain(0x09),
        Digit9 => Scancode::plain(0x0a),
        Digit0 => Scancode::plain(0x0b),
        Minus => Scancode::plain(0x0c),
        Equal => Scancode::plain(0x0d),
        Backspace => Scancode::plain(0x0e),
        Tab => Scancode::plain(0x0f),
        KeyQ => Scancode::plain(0x10),
        KeyW => Scancode::plain(0x11),
        KeyE => Scancode::plain(0x12),
        KeyR => Scancode::plain(0x13),
        KeyT => Scancode::plain(0x14),
        KeyY => Scancode::plain(0x15),
        KeyU => Scancode::plain(0x16),
        KeyI => Scancode::plain(0x17),
        KeyO => Scancode::plain(0x18),
        KeyP => Scancode::plain(0x19),
        BracketLeft => Scancode::plain(0x1a),
        BracketRight => Scancode::plain(0x1b),
        Enter => Scancode::plain(0x1c),
        ControlLeft => Scancode::plain(0x1d),
        KeyA => Scancode::plain(0x1e),
        KeyS => Scancode::plain(0x1f),
        KeyD => Scancode::plain(0x20),
        KeyF => Scancode::plain(0x21),
        KeyG => Scancode::plain(0x22),
        KeyH => Scancode::plain(0x23),
        KeyJ => Scancode::plain(0x24),
        KeyK => Scancode::plain(0x25),
        KeyL => Scancode::plain(0x26),
        Semicolon => Scancode::plain(0x27),
        Quote => Scancode::plain(0x28),
        Backquote => Scancode::plain(0x29),
        ShiftLeft => Scancode::plain(0x2a),
        Backslash => Scancode::plain(0x2b),
        KeyZ => Scancode::plain(0x2c),
        KeyX => Scancode::plain(0x2d),
        KeyC => Scancode::plain(0x2e),
        KeyV => Scancode::plain(0x2f),
        KeyB => Scancode::plain(0x30),
        KeyN => Scancode::plain(0x31),
        KeyM => Scancode::plain(0x32),
        Comma => Scancode::plain(0x33),
        Period => Scancode::plain(0x34),
        Slash => Scancode::plain(0x35),
        ShiftRight => Scancode::plain(0x36),
        NumpadMultiply => Scancode::plain(0x37),
        AltLeft => Scancode::plain(0x38),
        Space => Scancode::plain(0x39),
        CapsLock => Scancode::plain(0x3a),
        F1 => Scancode::plain(0x3b),
        F2 => Scancode::plain(0x3c),
        F3 => Scancode::plain(0x3d),
        F4 => Scancode::plain(0x3e),
        F5 => Scancode::plain(0x3f),
        F6 => Scancode::plain(0x40),
        F7 => Scancode::plain(0x41),
        F8 => Scancode::plain(0x42),
        F9 => Scancode::plain(0x43),
        F10 => Scancode::plain(0x44),
        NumLock => Scancode::plain(0x45),
        ScrollLock => Scancode::plain(0x46),
        Numpad7 => Scancode::plain(0x47),
        Numpad8 => Scancode::plain(0x48),
        Numpad9 => Scancode::plain(0x49),
        NumpadSubtract => Scancode::plain(0x4a),
        Numpad4 => Scancode::plain(0x4b),
        Numpad5 => Scancode::plain(0x4c),
        Numpad6 => Scancode::plain(0x4d),
        NumpadAdd => Scancode::plain(0x4e),
        Numpad1 => Scancode::plain(0x4f),
        Numpad2 => Scancode::plain(0x50),
        Numpad3 => Scancode::plain(0x51),
        Numpad0 => Scancode::plain(0x52),
        NumpadDecimal => Scancode::plain(0x53),
        F11 => Scancode::plain(0x57),
        F12 => Scancode::plain(0x58),
        NumpadEnter => Scancode::extended(0x1c),
        ControlRight => Scancode::extended(0x1d),
        NumpadDivide => Scancode::extended(0x35),
        AltRight => Scancode::extended(0x38),
        Home => Scancode::extended(0x47),
        ArrowUp => Scancode::extended(0x48),
        PageUp => Scancode::extended(0x49),
        ArrowLeft => Scancode::extended(0x4b),
        ArrowRight => Scancode::extended(0x4d),
        End => Scancode::extended(0x4f),
        ArrowDown => Scancode::extended(0x50),
        PageDown => Scancode::extended(0x51),
        Insert => Scancode::extended(0x52),
        Delete => Scancode::extended(0x53),
        SuperLeft | SuperRight => Scancode::extended(0x5b),
        ContextMenu => Scancode::extended(0x5d),
        _ => return None,
    };
    Some(scancode)
}

impl Scancode {
    const fn plain(code: u16) -> Self {
        Self {
            code,
            extended: false,
        }
    }

    const fn extended(code: u16) -> Self {
        Self {
            code,
            extended: true,
        }
    }
}

/// Convert winit key state and scancode to protocol key flags.
#[must_use]
pub fn key_flags(state: ElementState, extended: bool) -> u8 {
    let mut flags = 0;
    if state.is_pressed() {
        flags |= key_flag::PRESSED;
    }
    if extended {
        flags |= key_flag::EXTENDED;
    }
    flags
}

/// Update protocol pointer button bitmask from one winit mouse button event.
#[must_use]
pub fn update_buttons(current: u8, button: MouseButton, state: ElementState) -> u8 {
    let bit = match button {
        MouseButton::Left => pointer_button::LEFT,
        MouseButton::Right => pointer_button::RIGHT,
        MouseButton::Middle => pointer_button::MIDDLE,
        MouseButton::Back => pointer_button::X1,
        MouseButton::Forward => pointer_button::X2,
        MouseButton::Other(_) => 0,
    };
    if state.is_pressed() {
        current | bit
    } else {
        current & !bit
    }
}

/// Convert winit modifier state to protocol modifier bits.
#[must_use]
pub fn modifiers(state: ModifiersState) -> u16 {
    let mut out = 0;
    if state.shift_key() {
        out |= modifier::SHIFT;
    }
    if state.control_key() {
        out |= modifier::CTRL;
    }
    if state.alt_key() {
        out |= modifier::ALT;
    }
    if state.super_key() {
        out |= modifier::META;
    }
    out
}

/// Lock-state reporting is backend-specific in winit 0.30, so v1 reports no locks.
#[must_use]
pub fn locks() -> u8 {
    let _ = lock_state::CAPS | lock_state::NUM | lock_state::SCROLL;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_to_scancode_maps_plain_extended_and_unknown_keys() {
        assert_eq!(
            keycode_to_scancode(KeyCode::KeyA),
            Some(Scancode {
                code: 0x1e,
                extended: false,
            })
        );
        assert_eq!(
            keycode_to_scancode(KeyCode::ArrowLeft),
            Some(Scancode {
                code: 0x4b,
                extended: true,
            })
        );
        assert_eq!(keycode_to_scancode(KeyCode::Fn), None);
    }

    #[test]
    fn key_flags_encode_pressed_and_extended_bits() {
        assert_eq!(
            key_flags(ElementState::Pressed, true),
            key_flag::PRESSED | key_flag::EXTENDED
        );
        assert_eq!(key_flags(ElementState::Released, false), 0);
    }

    #[test]
    fn update_buttons_sets_and_clears_known_buttons_and_ignores_other() {
        let buttons = update_buttons(0, MouseButton::Left, ElementState::Pressed);
        let buttons = update_buttons(buttons, MouseButton::Back, ElementState::Pressed);
        assert_eq!(buttons, pointer_button::LEFT | pointer_button::X1);

        let buttons = update_buttons(buttons, MouseButton::Left, ElementState::Released);
        assert_eq!(buttons, pointer_button::X1);

        assert_eq!(
            update_buttons(buttons, MouseButton::Other(9), ElementState::Pressed),
            buttons
        );
    }

    #[test]
    fn modifiers_convert_winit_bits_to_protocol_bits() {
        let state = ModifiersState::SHIFT | ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(
            modifiers(state),
            modifier::SHIFT | modifier::CTRL | modifier::ALT
        );
        assert_eq!(modifiers(ModifiersState::SUPER), modifier::META);
        assert_eq!(modifiers(ModifiersState::empty()), 0);
    }

    #[test]
    fn locks_reports_no_lock_state_in_v1() {
        assert_eq!(locks(), 0);
    }
}
