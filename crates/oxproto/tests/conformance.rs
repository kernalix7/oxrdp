//! Byte-exact wire fixtures for the oxproto v1 format.
//!
//! Each fixture's expected bytes are written down by hand from
//! `docs/design/OXPROTO.md` (framing §3, type registry §5, conventions §6) and the
//! field order in `crates/oxproto/src/message/` — never produced by the encoder. An
//! accidental change to any message body is caught here instead of at interop time.
//!
//! Run with: `cargo test -p oxproto --test conformance`.

use oxproto::{
    channel, decode, encode_vec,
    message::{
        msg_type, Close, CursorVisibility, FrameAck, Message, ModifierSync, Ping, Pong,
        WindowClosed, WindowGeometry, WindowZOrder,
    },
};

/// `write_u64` emits the low 32 bits first (little-endian), then the high 32 bits.
const fn u64_le(v: u64) -> [u8; 8] {
    [
        (v & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        ((v >> 16) & 0xFF) as u8,
        ((v >> 24) & 0xFF) as u8,
        ((v >> 32) & 0xFF) as u8,
        ((v >> 40) & 0xFF) as u8,
        ((v >> 48) & 0xFF) as u8,
        ((v >> 56) & 0xFF) as u8,
    ]
}

/// Round-trip a body: assert the encoder emits exactly `expected` and that decoding
/// the result returns the original value.
macro_rules! body_fixture {
    ($name:ident, $ty:ty, $value:expr, $expected:expr) => {
        #[test]
        fn $name() {
            let value: $ty = $value;
            let expected: &[u8] = &$expected;
            let bytes = encode_vec(&value).expect("encode");
            assert_eq!(
                bytes.as_slice(),
                expected,
                "wire bytes diverged from the hardcoded fixture"
            );
            let back: $ty = decode::<$ty>(&bytes).expect("decode");
            assert_eq!(back, value, "decoded value does not match the original");
        }
    };
}

// --- Control-channel messages ------------------------------------------------

body_fixture!(
    ping_bytes,
    Ping,
    Ping {
        seq: 0x0403_0201,
        sent_us: 0x0807_0605_0403_0201,
    },
    [
        // seq: u32 LE (0x04030201 → lo byte first)
        0x01, 0x02, 0x03, 0x04, // sent_us: u64 LE (lo32, hi32)
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
    ]
);

body_fixture!(
    pong_bytes,
    Pong,
    Pong {
        seq: 0x0403_0201,
        sent_us: 0x0807_0605_0403_0201,
        agent_us: 0x100F_0E0D_0C0B_0A09,
    },
    [
        // seq: u32 LE
        0x01, 0x02, 0x03, 0x04, // sent_us: u64 LE
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // agent_us: u64 LE
        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10,
    ]
);

body_fixture!(
    close_bytes,
    Close,
    Close { reason: 0x0301 },
    [
        // reason: u16 LE
        0x01, 0x03,
    ]
);

// --- Window-lifecycle messages -----------------------------------------------

body_fixture!(
    window_closed_bytes,
    WindowClosed,
    WindowClosed {
        window_id: 0xDEAD_BEEF,
    },
    [
        // window_id: u32 LE
        0xEF, 0xBE, 0xAD, 0xDE,
    ]
);

body_fixture!(
    window_geometry_bytes,
    WindowGeometry,
    WindowGeometry {
        window_id: 0x1234_5678,
        x: -100,
        y: 200,
        width: 1024,
        height: 768,
    },
    [
        // window_id: u32 LE
        0x78, 0x56, 0x34, 0x12, // x: i32 LE (-100 = 0xFFFFFF9C)
        0x9C, 0xFF, 0xFF, 0xFF, // y: i32 LE (200 = 0x000000C8)
        0xC8, 0x00, 0x00, 0x00, // width: u16 LE (1024 = 0x0400)
        0x00, 0x04, // height: u16 LE (768 = 0x0300)
        0x00, 0x03,
    ]
);

body_fixture!(
    window_zorder_bytes,
    WindowZOrder,
    WindowZOrder {
        window_id: 0xCAFE_BABE,
        above_window_id: 0x0000_0007,
    },
    [
        // window_id: u32 LE
        0xBE, 0xBA, 0xFE, 0xCA, // above_window_id: u32 LE
        0x07, 0x00, 0x00, 0x00,
    ]
);

// --- Input-channel messages --------------------------------------------------

body_fixture!(
    modifier_sync_bytes,
    ModifierSync,
    ModifierSync {
        // SHIFT | CTRL | META = 1 | 2 | 8 = 0x000B
        modifiers: 0x000B,
        // CAPS | NUM = 1 | 2 = 0x03
        locks: 0x03,
    },
    [
        // modifiers: u16 LE
        0x0B, 0x00, // locks: u8
        0x03,
    ]
);

// --- Cursor-channel messages -------------------------------------------------

body_fixture!(
    cursor_visibility_true_bytes,
    CursorVisibility,
    CursorVisibility { visible: true },
    [
        // visible: bool as u8
        0x01,
    ]
);

body_fixture!(
    cursor_visibility_false_bytes,
    CursorVisibility,
    CursorVisibility { visible: false },
    [
        // visible: bool as u8
        0x00,
    ]
);

// --- Video flow control ------------------------------------------------------

body_fixture!(
    frame_ack_bytes,
    FrameAck,
    FrameAck {
        window_id: 0x1122_3344,
        frame_id: 0x8877_6655_4433_2211,
        decoded_us: 0x0A0B_0C0D_0E0F_1011,
        presented_us: 0x0102_0304_0506_0708,
    },
    [
        // window_id: u32 LE
        0x44, 0x33, 0x22, 0x11, // frame_id: u64 LE
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, // decoded_us: u64 LE
        0x11, 0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A, // presented_us: u64 LE
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01,
    ]
);

// --- Chunk envelope ----------------------------------------------------------

#[test]
fn ping_to_chunks_produces_one_documented_chunk() {
    // `Message::Ping{seq:1,sent_us:2}` (channel 0 = control). The body fits a single
    // chunk so there is exactly one, and its first 8 bytes are the documented header
    // (type u8, flags u8, channel u16 LE, length u32 LE) followed by the body.
    let msg = Message::Ping(Ping { seq: 1, sent_us: 2 });
    let chunks = msg.to_chunks(0).expect("to_chunks");
    assert_eq!(chunks.len(), 1, "a Ping fits in one chunk");

    let chunk = &chunks[0];
    let (header, body) = chunk.split_at(oxproto::CHUNK_HEADER_LEN);

    // Documented header (§3): type, flags, channel (u16 LE), length (u32 LE).
    assert_eq!(
        header,
        &[
            msg_type::PING, // type
            0x00,           // flags — single chunk, FRAG_MORE clear
            channel::CONTROL as u8,
            0x00, // channel = 0 (u16 LE)
            12,
            0x00,
            0x00,
            0x00, // length = 12 (u32 LE)
        ],
        "chunk header does not match the documented framing"
    );

    // Body follows the header: Ping{seq:1, sent_us:2}.
    let sent_us = u64_le(2);
    let mut expected_body = [0u8; 12];
    expected_body[0..4].copy_from_slice(&1u32.to_le_bytes());
    expected_body[4..12].copy_from_slice(&sent_us);
    assert_eq!(
        body,
        &expected_body[..],
        "chunk body does not match the Ping body"
    );
}

// --- Type-registry permanence -----------------------------------------------

/// The type registry (`OXPROTO.md` §5) is a permanent wire contract: a renumbering
/// would silently break every peer. Assert each constant keeps its documented code.
#[test]
fn type_registry_matches_spec() {
    assert_eq!(msg_type::CLIENT_HELLO, 0x01);
    assert_eq!(msg_type::SERVER_HELLO, 0x02);
    assert_eq!(msg_type::ERROR, 0x03);
    assert_eq!(msg_type::CLOSE, 0x04);
    assert_eq!(msg_type::PING, 0x05);
    assert_eq!(msg_type::PONG, 0x06);
    assert_eq!(msg_type::QUALITY_HINT, 0x07);
    assert_eq!(msg_type::DISPLAY_LAYOUT, 0x08);

    assert_eq!(msg_type::WINDOW_OPENED, 0x10);
    assert_eq!(msg_type::WINDOW_CLOSED, 0x11);
    assert_eq!(msg_type::WINDOW_GEOMETRY, 0x12);
    assert_eq!(msg_type::WINDOW_TITLE, 0x13);
    assert_eq!(msg_type::WINDOW_STATE, 0x14);
    assert_eq!(msg_type::WINDOW_ZORDER, 0x15);
    assert_eq!(msg_type::WINDOW_ICON, 0x16);

    assert_eq!(msg_type::FRAME_DATA, 0x20);
    assert_eq!(msg_type::FRAME_ACK, 0x21);

    assert_eq!(msg_type::POINTER_EVENT, 0x30);
    assert_eq!(msg_type::KEY_EVENT, 0x31);
    assert_eq!(msg_type::TEXT_INPUT, 0x32);
    assert_eq!(msg_type::MODIFIER_SYNC, 0x33);
    assert_eq!(msg_type::WINDOW_CONTROL, 0x38);

    assert_eq!(msg_type::CURSOR_SHAPE, 0x40);
    assert_eq!(msg_type::CURSOR_POSITION, 0x41);
    assert_eq!(msg_type::CURSOR_VISIBILITY, 0x42);
}
