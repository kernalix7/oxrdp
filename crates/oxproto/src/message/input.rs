//! Input and cursor messages (`docs/design/OXPROTO.md` §13, §14).

use oxrdp_pdu::{Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor};

use crate::wire::{
    read_blob, read_bool, read_i16, read_i32, read_string, read_u64, string_size, write_blob,
    write_bool, write_i16, write_i32, write_string, write_u64,
};

/// Bits in `PointerEvent.buttons`.
pub mod pointer_button {
    /// Left button held.
    pub const LEFT: u8 = 1 << 0;
    /// Right button held.
    pub const RIGHT: u8 = 1 << 1;
    /// Middle button held.
    pub const MIDDLE: u8 = 1 << 2;
    /// First extra (back) button held.
    pub const X1: u8 = 1 << 3;
    /// Second extra (forward) button held.
    pub const X2: u8 = 1 << 4;
}

/// Bits in `KeyEvent.flags`.
pub mod key_flag {
    /// The key is being pressed (cleared for a release).
    pub const PRESSED: u8 = 1 << 0;
    /// The scancode is an extended (E0-prefixed) code.
    pub const EXTENDED: u8 = 1 << 1;
}

/// Bits in `ModifierSync.modifiers`.
pub mod modifier {
    /// Either shift key.
    pub const SHIFT: u16 = 1 << 0;
    /// Either control key.
    pub const CTRL: u16 = 1 << 1;
    /// Either alt key.
    pub const ALT: u16 = 1 << 2;
    /// Either meta/Windows key.
    pub const META: u16 = 1 << 3;
}

/// Bits in `ModifierSync.locks`.
pub mod lock_state {
    /// Caps lock is on.
    pub const CAPS: u8 = 1 << 0;
    /// Num lock is on.
    pub const NUM: u8 = 1 << 1;
    /// Scroll lock is on.
    pub const SCROLL: u8 = 1 << 2;
}

/// Values for `WindowControl.action`.
pub mod window_action {
    /// Ask the app to close the window.
    pub const CLOSE: u8 = 1;
    /// Bring the window to the foreground.
    pub const ACTIVATE: u8 = 2;
    /// Minimize.
    pub const MINIMIZE: u8 = 3;
    /// Maximize.
    pub const MAXIMIZE: u8 = 4;
    /// Restore from minimized/maximized.
    pub const RESTORE: u8 = 5;
    /// Move to the supplied position.
    pub const MOVE: u8 = 6;
    /// Resize to the supplied size.
    pub const RESIZE: u8 = 7;
}

/// Values for `CursorShape.format`.
pub mod cursor_format {
    /// 32-bit BGRA, premultiplied alpha, top-down.
    pub const BGRA_PREMUL: u8 = 1;
}

/// Pointer motion, button, and wheel event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerEvent {
    /// Target window identifier.
    pub window_id: u32,
    /// Window-relative X coordinate.
    pub x: i32,
    /// Window-relative Y coordinate.
    pub y: i32,
    /// Bitmask of held pointer buttons.
    pub buttons: u8,
    /// Horizontal wheel delta in 1/120 of a notch.
    pub wheel_x: i16,
    /// Vertical wheel delta in 1/120 of a notch.
    pub wheel_y: i16,
    /// Event timestamp in microseconds.
    pub timestamp: u64,
}

impl Encode for PointerEvent {
    fn size(&self) -> usize {
        4 + 4 + 4 + 1 + 2 + 2 + 8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "PointerEvent.window_id")?;
        write_i32(dst, self.x, "PointerEvent.x")?;
        write_i32(dst, self.y, "PointerEvent.y")?;
        dst.write_u8(self.buttons, "PointerEvent.buttons")?;
        write_i16(dst, self.wheel_x, "PointerEvent.wheel_x")?;
        write_i16(dst, self.wheel_y, "PointerEvent.wheel_y")?;
        write_u64(dst, self.timestamp, "PointerEvent.timestamp")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for PointerEvent {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("PointerEvent.window_id")?,
            x: read_i32(src, "PointerEvent.x")?,
            y: read_i32(src, "PointerEvent.y")?,
            buttons: src.read_u8("PointerEvent.buttons")?,
            wheel_x: read_i16(src, "PointerEvent.wheel_x")?,
            wheel_y: read_i16(src, "PointerEvent.wheel_y")?,
            timestamp: read_u64(src, "PointerEvent.timestamp")?,
        })
    }
}

/// Keyboard key press or release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// PS/2 set 1 scancode.
    pub scancode: u16,
    /// Key event flags.
    pub flags: u8,
    /// Event timestamp in microseconds.
    pub timestamp: u64,
}

impl KeyEvent {
    /// Whether this event is a press (as opposed to a release).
    pub fn is_pressed(&self) -> bool {
        self.flags & key_flag::PRESSED != 0
    }
}

impl Encode for KeyEvent {
    fn size(&self) -> usize {
        2 + 1 + 8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.scancode, "KeyEvent.scancode")?;
        dst.write_u8(self.flags, "KeyEvent.flags")?;
        write_u64(dst, self.timestamp, "KeyEvent.timestamp")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for KeyEvent {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            scancode: src.read_u16_le("KeyEvent.scancode")?,
            flags: src.read_u8("KeyEvent.flags")?,
            timestamp: read_u64(src, "KeyEvent.timestamp")?,
        })
    }
}

/// UTF-8 text input from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    /// The text to insert.
    pub text: String,
}

impl Encode for TextInput {
    fn size(&self) -> usize {
        string_size(&self.text)
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        write_string(dst, &self.text, "TextInput.text")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for TextInput {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            text: read_string(src, "TextInput.text")?,
        })
    }
}

/// Synchronization of keyboard modifiers and lock states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifierSync {
    /// Bitmask of active modifiers.
    pub modifiers: u16,
    /// Bitmask of active lock states.
    pub locks: u8,
}

impl Encode for ModifierSync {
    fn size(&self) -> usize {
        2 + 1
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.modifiers, "ModifierSync.modifiers")?;
        dst.write_u8(self.locks, "ModifierSync.locks")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ModifierSync {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            modifiers: src.read_u16_le("ModifierSync.modifiers")?,
            locks: src.read_u8("ModifierSync.locks")?,
        })
    }
}

/// Request to manipulate a remote window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowControl {
    /// Target window identifier.
    pub window_id: u32,
    /// Action to perform.
    pub action: u8,
    /// X coordinate for move actions.
    pub x: i32,
    /// Y coordinate for move actions.
    pub y: i32,
    /// Width for resize actions.
    pub width: u16,
    /// Height for resize actions.
    pub height: u16,
}

impl Encode for WindowControl {
    fn size(&self) -> usize {
        4 + 1 + 4 + 4 + 2 + 2
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowControl.window_id")?;
        dst.write_u8(self.action, "WindowControl.action")?;
        write_i32(dst, self.x, "WindowControl.x")?;
        write_i32(dst, self.y, "WindowControl.y")?;
        dst.write_u16_le(self.width, "WindowControl.width")?;
        dst.write_u16_le(self.height, "WindowControl.height")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowControl {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowControl.window_id")?,
            action: src.read_u8("WindowControl.action")?,
            x: read_i32(src, "WindowControl.x")?,
            y: read_i32(src, "WindowControl.y")?,
            width: src.read_u16_le("WindowControl.width")?,
            height: src.read_u16_le("WindowControl.height")?,
        })
    }
}

/// Cursor shape definition sent to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    /// Cursor identifier.
    pub cursor_id: u32,
    /// Cursor width in pixels.
    pub width: u16,
    /// Cursor height in pixels.
    pub height: u16,
    /// Hotspot X coordinate.
    pub hotspot_x: u16,
    /// Hotspot Y coordinate.
    pub hotspot_y: u16,
    /// Pixel format of `data`.
    pub format: u8,
    /// Raw pixel data.
    pub data: Vec<u8>,
}

impl Encode for CursorShape {
    fn size(&self) -> usize {
        4 + 2 + 2 + 2 + 2 + 1 + 4 + self.data.len()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.cursor_id, "CursorShape.cursor_id")?;
        dst.write_u16_le(self.width, "CursorShape.width")?;
        dst.write_u16_le(self.height, "CursorShape.height")?;
        dst.write_u16_le(self.hotspot_x, "CursorShape.hotspot_x")?;
        dst.write_u16_le(self.hotspot_y, "CursorShape.hotspot_y")?;
        dst.write_u8(self.format, "CursorShape.format")?;
        write_blob(dst, &self.data, "CursorShape.data")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for CursorShape {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            cursor_id: src.read_u32_le("CursorShape.cursor_id")?,
            width: src.read_u16_le("CursorShape.width")?,
            height: src.read_u16_le("CursorShape.height")?,
            hotspot_x: src.read_u16_le("CursorShape.hotspot_x")?,
            hotspot_y: src.read_u16_le("CursorShape.hotspot_y")?,
            format: src.read_u8("CursorShape.format")?,
            data: read_blob(src, "CursorShape.data")?,
        })
    }
}

/// Cursor position update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPosition {
    /// Window identifier the position is relative to.
    pub window_id: u32,
    /// Window-relative X coordinate.
    pub x: i32,
    /// Window-relative Y coordinate.
    pub y: i32,
}

impl Encode for CursorPosition {
    fn size(&self) -> usize {
        4 + 4 + 4
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "CursorPosition.window_id")?;
        write_i32(dst, self.x, "CursorPosition.x")?;
        write_i32(dst, self.y, "CursorPosition.y")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for CursorPosition {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("CursorPosition.window_id")?,
            x: read_i32(src, "CursorPosition.x")?,
            y: read_i32(src, "CursorPosition.y")?,
        })
    }
}

/// Cursor visibility update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorVisibility {
    /// Whether the cursor is visible.
    pub visible: bool,
}

impl Encode for CursorVisibility {
    fn size(&self) -> usize {
        1
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        write_bool(dst, self.visible, "CursorVisibility.visible")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for CursorVisibility {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            visible: read_bool(src, "CursorVisibility.visible")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdp_pdu::{decode, encode_vec};

    #[test]
    fn pointer_event_round_trip() {
        let m = PointerEvent {
            window_id: 7,
            x: -12,
            y: 340,
            buttons: pointer_button::LEFT | pointer_button::X2,
            wheel_x: 0,
            wheel_y: -120,
            timestamp: 987_654_321,
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), 25);
        assert_eq!(decode::<PointerEvent>(&bytes).unwrap(), m);
    }

    #[test]
    fn key_event_round_trip() {
        let m = KeyEvent {
            scancode: 0x1C,
            flags: key_flag::PRESSED | key_flag::EXTENDED,
            timestamp: 42,
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), 11);
        assert!(m.is_pressed());
        assert_eq!(decode::<KeyEvent>(&bytes).unwrap(), m);

        let up = KeyEvent {
            scancode: 0x1C,
            flags: 0,
            timestamp: 43,
        };
        assert!(!up.is_pressed());
    }

    #[test]
    fn text_and_modifiers_round_trip() {
        let t = TextInput {
            text: "한글 입력".into(),
        };
        assert_eq!(decode::<TextInput>(&encode_vec(&t).unwrap()).unwrap(), t);

        let m = ModifierSync {
            modifiers: modifier::SHIFT | modifier::CTRL,
            locks: lock_state::CAPS,
        };
        assert_eq!(encode_vec(&m).unwrap().len(), 3);
        assert_eq!(decode::<ModifierSync>(&encode_vec(&m).unwrap()).unwrap(), m);
    }

    #[test]
    fn window_control_round_trip() {
        let m = WindowControl {
            window_id: 7,
            action: window_action::RESIZE,
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), 17);
        assert_eq!(decode::<WindowControl>(&bytes).unwrap(), m);
    }

    #[test]
    fn cursor_messages_round_trip() {
        let s = CursorShape {
            cursor_id: 3,
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 1,
            format: cursor_format::BGRA_PREMUL,
            data: vec![9u8; 16],
        };
        let bytes = encode_vec(&s).unwrap();
        assert_eq!(bytes.len(), s.size());
        assert_eq!(decode::<CursorShape>(&bytes).unwrap(), s);

        let p = CursorPosition {
            window_id: 7,
            x: 5,
            y: -9,
        };
        assert_eq!(encode_vec(&p).unwrap().len(), 12);
        assert_eq!(
            decode::<CursorPosition>(&encode_vec(&p).unwrap()).unwrap(),
            p
        );

        let v = CursorVisibility { visible: false };
        assert_eq!(encode_vec(&v).unwrap(), vec![0u8]);
        assert_eq!(
            decode::<CursorVisibility>(&[1]).unwrap(),
            CursorVisibility { visible: true }
        );
    }

    #[test]
    fn negative_coordinates_survive() {
        let m = CursorPosition {
            window_id: 0,
            x: i32::MIN,
            y: -1,
        };
        assert_eq!(
            decode::<CursorPosition>(&encode_vec(&m).unwrap()).unwrap(),
            m
        );
    }
}
