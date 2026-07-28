//! Message type registry and the [`Message`] dispatch enum.
//!
//! Bodies live in the submodules by area; this module owns the type-code registry
//! (`docs/design/OXPROTO.md` §5) and turns a `(type, body bytes)` pair — what
//! [`crate::envelope::Reassembler`] produces — into a typed [`Message`].

pub mod control;
pub mod input;
pub mod window;

use oxrdp_pdu::{decode, encode_vec, DecodeError, DecodeResult, EncodeResult};

pub use control::{
    ClientHello, Close, DisplayLayout, Error, Output, Ping, Pong, QualityHint, ServerHello,
};
pub use input::{
    CursorPosition, CursorShape, CursorVisibility, KeyEvent, ModifierSync, PointerEvent, TextInput,
    WindowControl,
};
pub use window::{
    FrameAck, FrameData, WindowClosed, WindowGeometry, WindowIcon, WindowOpened, WindowState,
    WindowTitle, WindowZOrder,
};

use crate::envelope::channel;

/// Message type codes. This registry is permanent: a retired code is never reused.
pub mod msg_type {
    /// Client handshake.
    pub const CLIENT_HELLO: u8 = 0x01;
    /// Agent handshake reply.
    pub const SERVER_HELLO: u8 = 0x02;
    /// Protocol or runtime error.
    pub const ERROR: u8 = 0x03;
    /// Orderly shutdown.
    pub const CLOSE: u8 = 0x04;
    /// Liveness probe.
    pub const PING: u8 = 0x05;
    /// Liveness reply (also carries the agent clock).
    pub const PONG: u8 = 0x06;
    /// Client's quality/latency preference.
    pub const QUALITY_HINT: u8 = 0x07;
    /// Client's output topology.
    pub const DISPLAY_LAYOUT: u8 = 0x08;

    /// A window appeared.
    pub const WINDOW_OPENED: u8 = 0x10;
    /// A window disappeared.
    pub const WINDOW_CLOSED: u8 = 0x11;
    /// A window moved or resized.
    pub const WINDOW_GEOMETRY: u8 = 0x12;
    /// A window's title changed.
    pub const WINDOW_TITLE: u8 = 0x13;
    /// A window was minimized/maximized/restored.
    pub const WINDOW_STATE: u8 = 0x14;
    /// A window's stacking position changed.
    pub const WINDOW_ZORDER: u8 = 0x15;
    /// A window's icon.
    pub const WINDOW_ICON: u8 = 0x16;

    /// An encoded (or raw) frame.
    pub const FRAME_DATA: u8 = 0x20;
    /// Client acknowledgement of a presented frame.
    pub const FRAME_ACK: u8 = 0x21;

    /// Pointer motion / buttons / wheel.
    pub const POINTER_EVENT: u8 = 0x30;
    /// Key press or release (PS/2 set 1 scancode).
    pub const KEY_EVENT: u8 = 0x31;
    /// Unicode text (IME path).
    pub const TEXT_INPUT: u8 = 0x32;
    /// Authoritative modifier/lock state.
    pub const MODIFIER_SYNC: u8 = 0x33;
    /// Client-initiated window action (close/activate/move/resize/…).
    pub const WINDOW_CONTROL: u8 = 0x38;

    /// A cursor bitmap.
    pub const CURSOR_SHAPE: u8 = 0x40;
    /// Cursor position within a window.
    pub const CURSOR_POSITION: u8 = 0x41;
    /// Whether the cursor is visible.
    pub const CURSOR_VISIBILITY: u8 = 0x42;
}

/// Protocol version this build implements.
pub const PROTOCOL_VERSION: u16 = 1;
/// Oldest protocol version this build still accepts.
pub const MIN_SUPPORTED_VERSION: u16 = 1;

/// Capability bits negotiated in the handshake (`OXPROTO.md` §8). A feature is active only if
/// both peers advertise it.
pub mod feature {
    /// The cursor is streamed separately instead of being composited into frames.
    pub const CURSOR_STREAM: u64 = 1 << 0;
    /// Frames are acknowledged and the sender applies an in-flight budget.
    pub const FRAME_ACK: u64 = 1 << 1;
    /// Frames may carry damage rectangles.
    pub const DAMAGE_RECTS: u64 = 1 << 2;
    /// The client may close/move/resize/activate windows.
    pub const WINDOW_CONTROL: u64 = 1 << 3;
    /// Unicode text input in addition to scancodes.
    pub const TEXT_INPUT: u64 = 1 << 4;
    /// Window and application icons are sent.
    pub const ICONS: u64 = 1 << 5;
    /// Audio streaming.
    pub const AUDIO: u64 = 1 << 6;
    /// Clipboard exchange.
    pub const CLIPBOARD: u64 = 1 << 7;

    /// Everything this build implements today.
    pub const SUPPORTED: u64 = CURSOR_STREAM | FRAME_ACK | WINDOW_CONTROL | TEXT_INPUT | ICONS;
}

/// Codec identifiers (`OXPROTO.md` §9). Zero is invalid so an all-zero field cannot look like
/// a valid codec.
pub mod codec {
    /// Uncompressed BGRA8, top-down, tightly packed. Bring-up only.
    pub const RAW_BGRA: u8 = 1;
    /// Annex-B H.264. Payload framing (parameter sets, keyframe/IDR semantics, NAL
    /// delimiting) is pinned down in `OXPROTO.md` §9.1.
    pub const H264: u8 = 2;
    /// Annex-B H.265.
    pub const H265: u8 = 3;
    /// AV1.
    pub const AV1: u8 = 4;
}

/// Error codes carried by [`Error`] (`OXPROTO.md` §15).
pub mod error_code {
    /// Malformed or out-of-sequence message.
    pub const PROTOCOL: u16 = 1;
    /// Authentication token rejected.
    pub const AUTH_FAILED: u16 = 2;
    /// No mutually supported protocol version.
    pub const VERSION_MISMATCH: u16 = 3;
    /// No mutually supported codec.
    pub const UNSUPPORTED_CODEC: u16 = 4;
    /// The referenced window no longer exists.
    pub const WINDOW_GONE: u16 = 5;
    /// Capture failed on the guest.
    pub const CAPTURE_FAILED: u16 = 6;
    /// Unexpected internal failure.
    pub const INTERNAL: u16 = 7;
    /// A message exceeded the size limit for its type.
    pub const TOO_LARGE: u16 = 8;
}

/// A decoded protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Client handshake.
    ClientHello(ClientHello),
    /// Agent handshake reply.
    ServerHello(ServerHello),
    /// Protocol or runtime error.
    Error(Error),
    /// Orderly shutdown.
    Close(Close),
    /// Liveness probe.
    Ping(Ping),
    /// Liveness reply.
    Pong(Pong),
    /// Quality/latency preference.
    QualityHint(QualityHint),
    /// Client output topology.
    DisplayLayout(DisplayLayout),

    /// A window appeared.
    WindowOpened(WindowOpened),
    /// A window disappeared.
    WindowClosed(WindowClosed),
    /// A window moved or resized.
    WindowGeometry(WindowGeometry),
    /// A window's title changed.
    WindowTitle(WindowTitle),
    /// A window's show state changed.
    WindowState(WindowState),
    /// A window's stacking position changed.
    WindowZOrder(WindowZOrder),
    /// A window's icon.
    WindowIcon(WindowIcon),

    /// An encoded (or raw) frame.
    FrameData(FrameData),
    /// Acknowledgement of a presented frame.
    FrameAck(FrameAck),

    /// Pointer event.
    PointerEvent(PointerEvent),
    /// Key event.
    KeyEvent(KeyEvent),
    /// Unicode text.
    TextInput(TextInput),
    /// Modifier/lock state.
    ModifierSync(ModifierSync),
    /// Client-initiated window action.
    WindowControl(WindowControl),

    /// Cursor bitmap.
    CursorShape(CursorShape),
    /// Cursor position.
    CursorPosition(CursorPosition),
    /// Cursor visibility.
    CursorVisibility(CursorVisibility),
}

impl Message {
    /// The wire type code for this message.
    pub fn msg_type(&self) -> u8 {
        match self {
            Message::ClientHello(_) => msg_type::CLIENT_HELLO,
            Message::ServerHello(_) => msg_type::SERVER_HELLO,
            Message::Error(_) => msg_type::ERROR,
            Message::Close(_) => msg_type::CLOSE,
            Message::Ping(_) => msg_type::PING,
            Message::Pong(_) => msg_type::PONG,
            Message::QualityHint(_) => msg_type::QUALITY_HINT,
            Message::DisplayLayout(_) => msg_type::DISPLAY_LAYOUT,
            Message::WindowOpened(_) => msg_type::WINDOW_OPENED,
            Message::WindowClosed(_) => msg_type::WINDOW_CLOSED,
            Message::WindowGeometry(_) => msg_type::WINDOW_GEOMETRY,
            Message::WindowTitle(_) => msg_type::WINDOW_TITLE,
            Message::WindowState(_) => msg_type::WINDOW_STATE,
            Message::WindowZOrder(_) => msg_type::WINDOW_ZORDER,
            Message::WindowIcon(_) => msg_type::WINDOW_ICON,
            Message::FrameData(_) => msg_type::FRAME_DATA,
            Message::FrameAck(_) => msg_type::FRAME_ACK,
            Message::PointerEvent(_) => msg_type::POINTER_EVENT,
            Message::KeyEvent(_) => msg_type::KEY_EVENT,
            Message::TextInput(_) => msg_type::TEXT_INPUT,
            Message::ModifierSync(_) => msg_type::MODIFIER_SYNC,
            Message::WindowControl(_) => msg_type::WINDOW_CONTROL,
            Message::CursorShape(_) => msg_type::CURSOR_SHAPE,
            Message::CursorPosition(_) => msg_type::CURSOR_POSITION,
            Message::CursorVisibility(_) => msg_type::CURSOR_VISIBILITY,
        }
    }

    /// The channel this message belongs on (`OXPROTO.md` §4).
    ///
    /// Video rides the per-window channel the agent assigned in [`WindowOpened`], which the
    /// sender supplies; everything else has a fixed channel.
    pub fn channel(&self, video_channel: u16) -> u16 {
        match self {
            Message::FrameData(_) => video_channel,
            Message::PointerEvent(_)
            | Message::KeyEvent(_)
            | Message::TextInput(_)
            | Message::ModifierSync(_)
            | Message::WindowControl(_) => channel::INPUT,
            Message::CursorShape(_) | Message::CursorPosition(_) | Message::CursorVisibility(_) => {
                channel::CURSOR
            }
            Message::WindowOpened(_)
            | Message::WindowClosed(_)
            | Message::WindowGeometry(_)
            | Message::WindowTitle(_)
            | Message::WindowState(_)
            | Message::WindowZOrder(_)
            | Message::WindowIcon(_) => channel::WINDOW,
            _ => channel::CONTROL,
        }
    }

    /// Encode just the body (no chunk header); [`crate::envelope::fragment`] adds framing.
    pub fn encode_body(&self) -> EncodeResult<Vec<u8>> {
        match self {
            Message::ClientHello(m) => encode_vec(m),
            Message::ServerHello(m) => encode_vec(m),
            Message::Error(m) => encode_vec(m),
            Message::Close(m) => encode_vec(m),
            Message::Ping(m) => encode_vec(m),
            Message::Pong(m) => encode_vec(m),
            Message::QualityHint(m) => encode_vec(m),
            Message::DisplayLayout(m) => encode_vec(m),
            Message::WindowOpened(m) => encode_vec(m),
            Message::WindowClosed(m) => encode_vec(m),
            Message::WindowGeometry(m) => encode_vec(m),
            Message::WindowTitle(m) => encode_vec(m),
            Message::WindowState(m) => encode_vec(m),
            Message::WindowZOrder(m) => encode_vec(m),
            Message::WindowIcon(m) => encode_vec(m),
            Message::FrameData(m) => encode_vec(m),
            Message::FrameAck(m) => encode_vec(m),
            Message::PointerEvent(m) => encode_vec(m),
            Message::KeyEvent(m) => encode_vec(m),
            Message::TextInput(m) => encode_vec(m),
            Message::ModifierSync(m) => encode_vec(m),
            Message::WindowControl(m) => encode_vec(m),
            Message::CursorShape(m) => encode_vec(m),
            Message::CursorPosition(m) => encode_vec(m),
            Message::CursorVisibility(m) => encode_vec(m),
        }
    }

    /// Decode a reassembled `(type, body)` pair.
    ///
    /// A type this build does not know is **not** an error the caller must die on — see
    /// [`Message::decode_known`], which returns `None` so a receiver can skip it. This
    /// function is the strict form used where an unknown type really is a protocol violation.
    pub fn decode_body(msg_type: u8, body: &[u8]) -> DecodeResult<Self> {
        Self::decode_known(msg_type, body)?.ok_or(DecodeError::InvalidField {
            context: "oxproto message",
            field: "type",
            reason: "unknown message type",
        })
    }

    /// Decode a reassembled `(type, body)` pair, returning `Ok(None)` for a type this build
    /// does not implement. Forward compatibility depends on callers using this and skipping.
    pub fn decode_known(msg_type: u8, body: &[u8]) -> DecodeResult<Option<Self>> {
        let msg = match msg_type {
            msg_type::CLIENT_HELLO => Message::ClientHello(decode(body)?),
            msg_type::SERVER_HELLO => Message::ServerHello(decode(body)?),
            msg_type::ERROR => Message::Error(decode(body)?),
            msg_type::CLOSE => Message::Close(decode(body)?),
            msg_type::PING => Message::Ping(decode(body)?),
            msg_type::PONG => Message::Pong(decode(body)?),
            msg_type::QUALITY_HINT => Message::QualityHint(decode(body)?),
            msg_type::DISPLAY_LAYOUT => Message::DisplayLayout(decode(body)?),
            msg_type::WINDOW_OPENED => Message::WindowOpened(decode(body)?),
            msg_type::WINDOW_CLOSED => Message::WindowClosed(decode(body)?),
            msg_type::WINDOW_GEOMETRY => Message::WindowGeometry(decode(body)?),
            msg_type::WINDOW_TITLE => Message::WindowTitle(decode(body)?),
            msg_type::WINDOW_STATE => Message::WindowState(decode(body)?),
            msg_type::WINDOW_ZORDER => Message::WindowZOrder(decode(body)?),
            msg_type::WINDOW_ICON => Message::WindowIcon(decode(body)?),
            msg_type::FRAME_DATA => Message::FrameData(decode(body)?),
            msg_type::FRAME_ACK => Message::FrameAck(decode(body)?),
            msg_type::POINTER_EVENT => Message::PointerEvent(decode(body)?),
            msg_type::KEY_EVENT => Message::KeyEvent(decode(body)?),
            msg_type::TEXT_INPUT => Message::TextInput(decode(body)?),
            msg_type::MODIFIER_SYNC => Message::ModifierSync(decode(body)?),
            msg_type::WINDOW_CONTROL => Message::WindowControl(decode(body)?),
            msg_type::CURSOR_SHAPE => Message::CursorShape(decode(body)?),
            msg_type::CURSOR_POSITION => Message::CursorPosition(decode(body)?),
            msg_type::CURSOR_VISIBILITY => Message::CursorVisibility(decode(body)?),
            _ => return Ok(None),
        };
        Ok(Some(msg))
    }

    /// Encode this message into ready-to-send wire chunks.
    pub fn to_chunks(&self, video_channel: u16) -> EncodeResult<Vec<Vec<u8>>> {
        let body = self.encode_body()?;
        crate::envelope::fragment(self.msg_type(), self.channel(video_channel), &body)
    }
}
