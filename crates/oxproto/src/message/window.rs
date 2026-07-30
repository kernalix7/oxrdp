//! Window lifecycle and video messages (`docs/design/OXPROTO.md` §11, §12).

use oxrdp_pdu::{Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor};

use crate::wire::{
    read_blob, read_i32, read_string, read_u64, string_size, write_blob, write_i32, write_string,
    write_u64,
};

/// Bits in `WindowOpened.flags`.
pub mod window_flag {
    /// The window can be resized by the user.
    pub const RESIZABLE: u32 = 1 << 0;
    /// The window has a system frame/title bar.
    pub const HAS_FRAME: u32 = 1 << 1;
    /// The window is always on top.
    pub const TOPMOST: u32 = 1 << 2;
    /// The window is currently minimized.
    pub const MINIMIZED: u32 = 1 << 3;
    /// The window is currently maximized.
    pub const MAXIMIZED: u32 = 1 << 4;
}

/// Values for `WindowState.state`.
pub mod window_show {
    /// Restored / normal.
    pub const NORMAL: u8 = 0;
    /// Minimized.
    pub const MINIMIZED: u8 = 1;
    /// Maximized.
    pub const MAXIMIZED: u8 = 2;
}

/// Bits in `FrameData.flags`.
pub mod frame_flag {
    /// This frame is a keyframe / full refresh — decodable without any earlier frame for the
    /// window. Precise per-codec meaning (e.g. IDR, not any I-frame, for `H264`) is defined in
    /// `OXPROTO.md` §9.1.
    pub const KEYFRAME: u8 = 1 << 0;
}

/// A window has been opened on the remote side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowOpened {
    /// Unique window identifier.
    pub window_id: u32,
    /// Video channel assigned to this window.
    pub video_channel: u16,
    /// Process ID owning the window.
    pub pid: u32,
    /// Application identifier.
    pub app_id: String,
    /// Window title.
    pub title: String,
    /// Left edge screen coordinate.
    pub x: i32,
    /// Top edge screen coordinate.
    pub y: i32,
    /// Window width in pixels.
    pub width: u16,
    /// Window height in pixels.
    pub height: u16,
    /// Dots per inch.
    pub dpi: u16,
    /// Bitmask of `window_flag` values.
    pub flags: u32,
    /// Owning window id (0 if none).
    pub owner_id: u32,
}

impl Encode for WindowOpened {
    fn size(&self) -> usize {
        4 + 2 + 4 + string_size(&self.app_id) + string_size(&self.title) + 4 + 4 + 2 + 2 + 2 + 4 + 4
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowOpened.window_id")?;
        dst.write_u16_le(self.video_channel, "WindowOpened.video_channel")?;
        dst.write_u32_le(self.pid, "WindowOpened.pid")?;
        write_string(dst, &self.app_id, "WindowOpened.app_id")?;
        write_string(dst, &self.title, "WindowOpened.title")?;
        write_i32(dst, self.x, "WindowOpened.x")?;
        write_i32(dst, self.y, "WindowOpened.y")?;
        dst.write_u16_le(self.width, "WindowOpened.width")?;
        dst.write_u16_le(self.height, "WindowOpened.height")?;
        dst.write_u16_le(self.dpi, "WindowOpened.dpi")?;
        dst.write_u32_le(self.flags, "WindowOpened.flags")?;
        dst.write_u32_le(self.owner_id, "WindowOpened.owner_id")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowOpened {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowOpened.window_id")?,
            video_channel: src.read_u16_le("WindowOpened.video_channel")?,
            pid: src.read_u32_le("WindowOpened.pid")?,
            app_id: read_string(src, "WindowOpened.app_id")?,
            title: read_string(src, "WindowOpened.title")?,
            x: read_i32(src, "WindowOpened.x")?,
            y: read_i32(src, "WindowOpened.y")?,
            width: src.read_u16_le("WindowOpened.width")?,
            height: src.read_u16_le("WindowOpened.height")?,
            dpi: src.read_u16_le("WindowOpened.dpi")?,
            flags: src.read_u32_le("WindowOpened.flags")?,
            owner_id: src.read_u32_le("WindowOpened.owner_id")?,
        })
    }
}

/// A window has been closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowClosed {
    /// Unique window identifier.
    pub window_id: u32,
}

impl Encode for WindowClosed {
    fn size(&self) -> usize {
        4
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowClosed.window_id")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowClosed {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowClosed.window_id")?,
        })
    }
}

/// Window geometry update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowGeometry {
    /// Unique window identifier.
    pub window_id: u32,
    /// Left edge screen coordinate.
    pub x: i32,
    /// Top edge screen coordinate.
    pub y: i32,
    /// Window width in pixels.
    pub width: u16,
    /// Window height in pixels.
    pub height: u16,
}

impl Encode for WindowGeometry {
    fn size(&self) -> usize {
        16
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowGeometry.window_id")?;
        write_i32(dst, self.x, "WindowGeometry.x")?;
        write_i32(dst, self.y, "WindowGeometry.y")?;
        dst.write_u16_le(self.width, "WindowGeometry.width")?;
        dst.write_u16_le(self.height, "WindowGeometry.height")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowGeometry {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowGeometry.window_id")?,
            x: read_i32(src, "WindowGeometry.x")?,
            y: read_i32(src, "WindowGeometry.y")?,
            width: src.read_u16_le("WindowGeometry.width")?,
            height: src.read_u16_le("WindowGeometry.height")?,
        })
    }
}

/// Window title update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowTitle {
    /// Unique window identifier.
    pub window_id: u32,
    /// New window title.
    pub title: String,
}

impl Encode for WindowTitle {
    fn size(&self) -> usize {
        4 + string_size(&self.title)
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowTitle.window_id")?;
        write_string(dst, &self.title, "WindowTitle.title")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowTitle {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowTitle.window_id")?,
            title: read_string(src, "WindowTitle.title")?,
        })
    }
}

/// Window state update.
///
/// Sent whenever `state` or `flags` changes, not only on a show-state transition — see
/// `OXPROTO.md` §11 for exactly when and what a receiver must do with each field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowState {
    /// Unique window identifier.
    pub window_id: u32,
    /// Current show state (`window_show` value) — always the complete current state, never a
    /// delta.
    pub state: u8,
    /// Bitmask of `window_flag` values — the same meaning as `WindowOpened.flags`, and always
    /// the complete current bitmask, not a delta: a receiver replaces its stored flags with
    /// this value rather than merging it in.
    pub flags: u32,
}

impl Encode for WindowState {
    fn size(&self) -> usize {
        9
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowState.window_id")?;
        dst.write_u8(self.state, "WindowState.state")?;
        dst.write_u32_le(self.flags, "WindowState.flags")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowState {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowState.window_id")?,
            state: src.read_u8("WindowState.state")?,
            flags: src.read_u32_le("WindowState.flags")?,
        })
    }
}

/// Window z-order update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowZOrder {
    /// Unique window identifier.
    pub window_id: u32,
    /// Window id this one sits above (0 = bottom).
    pub above_window_id: u32,
}

impl Encode for WindowZOrder {
    fn size(&self) -> usize {
        8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowZOrder.window_id")?;
        dst.write_u32_le(self.above_window_id, "WindowZOrder.above_window_id")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowZOrder {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowZOrder.window_id")?,
            above_window_id: src.read_u32_le("WindowZOrder.above_window_id")?,
        })
    }
}

/// Window icon update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowIcon {
    /// Unique window identifier.
    pub window_id: u32,
    /// Icon width in pixels.
    pub width: u16,
    /// Icon height in pixels.
    pub height: u16,
    /// Icon pixels, **BGRA8 in memory order** (byte 0 = blue), straight — *not*
    /// premultiplied — alpha, top-down, tightly packed at `width * 4` bytes per row.
    ///
    /// Named by memory order, like every other pixel payload in this protocol (`RAW_BGRA`,
    /// `CursorShape`'s `BGRA_PREMUL`): it is what `GetDIBits` hands back on Windows, so the agent
    /// copies it without reordering. Note the difference from `CursorShape`, whose alpha *is*
    /// premultiplied.
    pub bgra: Vec<u8>,
}

impl Encode for WindowIcon {
    fn size(&self) -> usize {
        4 + 2 + 2 + 4 + self.bgra.len()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "WindowIcon.window_id")?;
        dst.write_u16_le(self.width, "WindowIcon.width")?;
        dst.write_u16_le(self.height, "WindowIcon.height")?;
        write_blob(dst, &self.bgra, "WindowIcon.bgra")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for WindowIcon {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("WindowIcon.window_id")?,
            width: src.read_u16_le("WindowIcon.width")?,
            height: src.read_u16_le("WindowIcon.height")?,
            bgra: read_blob(src, "WindowIcon.bgra")?.to_vec(),
        })
    }
}

/// Encoded video frame payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameData {
    /// Unique window identifier.
    pub window_id: u32,
    /// Monotonic frame identifier.
    pub frame_id: u64,
    /// Codec identifier.
    pub codec: u8,
    /// Bitmask of `frame_flag` values.
    pub flags: u8,
    /// Frame width in pixels.
    pub width: u16,
    /// Frame height in pixels.
    pub height: u16,
    /// Capture timestamp in microseconds.
    pub captured_us: u64,
    /// Timestamp, in microseconds, when the compressed bitstream for this frame became
    /// available. For a real encoder this is `>= captured_us`; see `OXPROTO.md` §9.1.
    pub encoded_us: u64,
    /// Encoded bytes for exactly one access unit (one picture). Codec-specific framing —
    /// NAL delimiting, parameter-set placement, keyframe semantics — is defined in
    /// `OXPROTO.md` §9, see §9.1 for `H264`.
    pub data: Vec<u8>,
}

impl FrameData {
    /// Whether this frame is a keyframe.
    pub fn is_keyframe(&self) -> bool {
        self.flags & frame_flag::KEYFRAME != 0
    }
}

impl Encode for FrameData {
    fn size(&self) -> usize {
        4 + 8 + 1 + 1 + 2 + 2 + 8 + 8 + 4 + self.data.len()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "FrameData.window_id")?;
        write_u64(dst, self.frame_id, "FrameData.frame_id")?;
        dst.write_u8(self.codec, "FrameData.codec")?;
        dst.write_u8(self.flags, "FrameData.flags")?;
        dst.write_u16_le(self.width, "FrameData.width")?;
        dst.write_u16_le(self.height, "FrameData.height")?;
        write_u64(dst, self.captured_us, "FrameData.captured_us")?;
        write_u64(dst, self.encoded_us, "FrameData.encoded_us")?;
        write_blob(dst, &self.data, "FrameData.data")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for FrameData {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("FrameData.window_id")?,
            frame_id: read_u64(src, "FrameData.frame_id")?,
            codec: src.read_u8("FrameData.codec")?,
            flags: src.read_u8("FrameData.flags")?,
            width: src.read_u16_le("FrameData.width")?,
            height: src.read_u16_le("FrameData.height")?,
            captured_us: read_u64(src, "FrameData.captured_us")?,
            encoded_us: read_u64(src, "FrameData.encoded_us")?,
            data: read_blob(src, "FrameData.data")?.to_vec(),
        })
    }
}

/// Acknowledgement of a decoded and presented frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameAck {
    /// Unique window identifier.
    pub window_id: u32,
    /// Frame identifier being acknowledged.
    pub frame_id: u64,
    /// Decode completion timestamp in microseconds.
    pub decoded_us: u64,
    /// Presentation timestamp in microseconds.
    pub presented_us: u64,
}

impl Encode for FrameAck {
    fn size(&self) -> usize {
        28
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u32_le(self.window_id, "FrameAck.window_id")?;
        write_u64(dst, self.frame_id, "FrameAck.frame_id")?;
        write_u64(dst, self.decoded_us, "FrameAck.decoded_us")?;
        write_u64(dst, self.presented_us, "FrameAck.presented_us")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for FrameAck {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        Ok(Self {
            window_id: src.read_u32_le("FrameAck.window_id")?,
            frame_id: read_u64(src, "FrameAck.frame_id")?,
            decoded_us: read_u64(src, "FrameAck.decoded_us")?,
            presented_us: read_u64(src, "FrameAck.presented_us")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdp_pdu::{decode, encode_vec};

    #[test]
    fn window_opened_round_trip() {
        let m = WindowOpened {
            window_id: 42,
            video_channel: 16,
            pid: 1234,
            app_id: "notepad.exe".into(),
            title: "Untitled - Notepad".into(),
            x: -100,
            y: 50,
            width: 800,
            height: 600,
            dpi: 144,
            flags: window_flag::RESIZABLE | window_flag::HAS_FRAME,
            owner_id: 0,
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), m.size());
        assert_eq!(decode::<WindowOpened>(&bytes).unwrap(), m);
    }

    #[test]
    fn window_events_round_trip() {
        let c = WindowClosed { window_id: 42 };
        assert_eq!(decode::<WindowClosed>(&encode_vec(&c).unwrap()).unwrap(), c);

        let g = WindowGeometry {
            window_id: 42,
            x: -5,
            y: 7,
            width: 1024,
            height: 768,
        };
        assert_eq!(encode_vec(&g).unwrap().len(), 16);
        assert_eq!(
            decode::<WindowGeometry>(&encode_vec(&g).unwrap()).unwrap(),
            g
        );

        let t = WindowTitle {
            window_id: 42,
            title: "새 문서".into(),
        };
        assert_eq!(decode::<WindowTitle>(&encode_vec(&t).unwrap()).unwrap(), t);

        let s = WindowState {
            window_id: 42,
            state: window_show::MAXIMIZED,
            flags: window_flag::MAXIMIZED,
        };
        assert_eq!(encode_vec(&s).unwrap().len(), 9);
        assert_eq!(decode::<WindowState>(&encode_vec(&s).unwrap()).unwrap(), s);

        let z = WindowZOrder {
            window_id: 42,
            above_window_id: 7,
        };
        assert_eq!(encode_vec(&z).unwrap().len(), 8);
        assert_eq!(decode::<WindowZOrder>(&encode_vec(&z).unwrap()).unwrap(), z);

        let i = WindowIcon {
            window_id: 42,
            width: 2,
            height: 2,
            bgra: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        };
        assert_eq!(encode_vec(&i).unwrap().len(), i.size());
        assert_eq!(decode::<WindowIcon>(&encode_vec(&i).unwrap()).unwrap(), i);
    }

    #[test]
    fn frame_data_round_trip() {
        let m = FrameData {
            window_id: 42,
            frame_id: 9_000_000_001,
            codec: 1,
            flags: frame_flag::KEYFRAME,
            width: 800,
            height: 600,
            captured_us: 111_222_333,
            encoded_us: 111_222_999,
            data: vec![0xAB; 64],
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), m.size());
        assert_eq!(bytes.len(), 4 + 8 + 1 + 1 + 2 + 2 + 8 + 8 + 4 + 64);
        assert!(m.is_keyframe());
        assert_eq!(decode::<FrameData>(&bytes).unwrap(), m);
    }

    #[test]
    fn frame_ack_round_trip() {
        let m = FrameAck {
            window_id: 42,
            frame_id: 9_000_000_001,
            decoded_us: 5,
            presented_us: 9,
        };
        let bytes = encode_vec(&m).unwrap();
        assert_eq!(bytes.len(), 28);
        assert_eq!(decode::<FrameAck>(&bytes).unwrap(), m);
    }

    #[test]
    fn empty_frame_payload_is_valid() {
        let m = FrameData {
            window_id: 1,
            frame_id: 0,
            codec: 1,
            flags: 0,
            width: 0,
            height: 0,
            captured_us: 0,
            encoded_us: 0,
            data: Vec::new(),
        };
        assert_eq!(decode::<FrameData>(&encode_vec(&m).unwrap()).unwrap(), m);
    }
}
