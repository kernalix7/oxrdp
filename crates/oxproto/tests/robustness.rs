//! The decoder is exposed to untrusted input by design: the agent parses whatever reaches its
//! listening socket. These tests assert the property that matters — **malformed input yields an
//! error, never a panic and never an unbounded allocation**.
//!
//! This is a deterministic smoke-fuzz that runs on stable in CI. `fuzz/` holds the cargo-fuzz
//! target for deeper, coverage-guided runs.

use oxproto::envelope::{ChunkHeader, Reassembler};
use oxproto::{decode, Message};

/// xorshift64* — a deterministic PRNG so a failure is always reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill(&mut self, buf: &mut Vec<u8>, len: usize) {
        buf.clear();
        while buf.len() < len {
            buf.extend_from_slice(&self.next().to_le_bytes());
        }
        buf.truncate(len);
    }
}

#[test]
fn arbitrary_bodies_never_panic() {
    let mut rng = Rng(0x0BAD_C0DE_DEAD_BEEF);
    let mut buf = Vec::new();

    for i in 0..20_000u32 {
        let len = (rng.next() % 96) as usize;
        rng.fill(&mut buf, len);
        // Sweep every type code, including unassigned ones.
        let msg_type = (i % 256) as u8;
        // The contract: Ok(Some), Ok(None) for an unknown type, or Err — but never a panic.
        let _ = Message::decode_known(msg_type, &buf);
    }
}

#[test]
fn arbitrary_chunk_headers_never_panic() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut buf = Vec::new();

    for _ in 0..20_000 {
        rng.fill(&mut buf, 8);
        let _ = decode::<ChunkHeader>(&buf);
    }
}

#[test]
fn truncating_a_valid_message_is_always_an_error_not_a_panic() {
    use oxproto::message::{ClientHello, DisplayLayout, Output};

    let msg = Message::ClientHello(ClientHello {
        version_min: 1,
        version_max: 1,
        features: u64::MAX,
        auth_token: "a-fairly-long-token-value".into(),
        client_name: "oxclient".into(),
        codecs: vec![1, 2, 3],
        display: DisplayLayout {
            outputs: vec![Output {
                id: 0,
                x: -1,
                y: -1,
                width: 1920,
                height: 1080,
                scale_num: 3,
                scale_den: 2,
                refresh_mhz: 143_980,
            }],
        },
    });
    let body = msg.encode_body().unwrap();

    // Every prefix shorter than the whole body must fail cleanly.
    for cut in 0..body.len() {
        let res = Message::decode_known(oxproto::msg_type::CLIENT_HELLO, &body[..cut]);
        assert!(res.is_err(), "truncation at {cut} should not decode");
    }
    // The complete body still round-trips.
    assert_eq!(
        Message::decode_known(oxproto::msg_type::CLIENT_HELLO, &body).unwrap(),
        Some(msg)
    );
}

#[test]
fn a_declared_length_cannot_make_the_receiver_allocate() {
    // A string claiming 65535 bytes inside a 4-byte body must fail immediately rather than
    // reserving 64 KiB — the pre-auth amplification the audit flagged.
    let body = [0xFF, 0xFF, 0x00, 0x00];
    assert!(Message::decode_known(oxproto::msg_type::TEXT_INPUT, &body).is_err());

    // Same for a blob length field.
    let mut frame = vec![0u8; 4 + 8 + 1 + 1 + 2 + 2 + 8 + 8];
    frame.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(Message::decode_known(oxproto::msg_type::FRAME_DATA, &frame).is_err());
}

#[test]
fn reassembly_rejects_a_flood_of_oversized_fragments() {
    // A peer that keeps sending FRAG_MORE chunks must hit the per-type cap instead of growing
    // the receiver's buffer without bound.
    let mut r = Reassembler::new();
    let chunk = vec![0u8; oxproto::MAX_CHUNK_PAYLOAD];
    let header = ChunkHeader {
        msg_type: oxproto::msg_type::WINDOW_TITLE, // 8 KiB cap
        flags: oxproto::envelope::chunk_flags::FRAG_MORE,
        channel: oxproto::channel::WINDOW,
        length: chunk.len() as u32,
    };

    // The very first oversized fragment is refused, and the channel is dropped.
    assert!(r.push(&header, &chunk).is_err());
    assert_eq!(r.pending_channels(), 0);
}

#[test]
fn every_known_type_round_trips_through_the_registry() {
    // Guards against a registry entry that encodes under one code and decodes under another.
    use oxproto::message::*;

    let messages = vec![
        Message::Close(Close {
            reason: close_reason::GOING_AWAY,
        }),
        Message::Ping(Ping { seq: 1, sent_us: 2 }),
        Message::Pong(Pong {
            seq: 1,
            sent_us: 2,
            agent_us: 3,
        }),
        Message::Error(Error {
            code: 7,
            message: "x".into(),
        }),
        Message::QualityHint(QualityHint {
            window_id: 0,
            target_fps: 60,
            max_bitrate_kbps: 1,
            flags: 0,
        }),
        Message::WindowClosed(WindowClosed { window_id: 1 }),
        Message::WindowGeometry(WindowGeometry {
            window_id: 1,
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }),
        Message::WindowTitle(WindowTitle {
            window_id: 1,
            title: "t".into(),
        }),
        Message::WindowState(WindowState {
            window_id: 1,
            state: 0,
            flags: 0,
        }),
        Message::WindowZOrder(WindowZOrder {
            window_id: 1,
            above_window_id: 0,
        }),
        Message::WindowIcon(WindowIcon {
            window_id: 1,
            width: 1,
            height: 1,
            bgra: vec![0, 0, 0, 0],
        }),
        Message::FrameAck(FrameAck {
            window_id: 1,
            frame_id: 2,
            decoded_us: 3,
            presented_us: 4,
        }),
        Message::PointerEvent(PointerEvent {
            window_id: 1,
            x: 1,
            y: 2,
            buttons: 3,
            wheel_x: 4,
            wheel_y: 5,
            timestamp: 6,
        }),
        Message::KeyEvent(KeyEvent {
            scancode: 1,
            flags: 1,
            timestamp: 2,
        }),
        Message::TextInput(TextInput { text: "가".into() }),
        Message::ModifierSync(ModifierSync {
            modifiers: 1,
            locks: 2,
        }),
        Message::WindowControl(WindowControl {
            window_id: 1,
            action: 1,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }),
        Message::CursorPosition(CursorPosition {
            window_id: 1,
            x: 1,
            y: 2,
        }),
        Message::CursorVisibility(CursorVisibility { visible: true }),
    ];

    for msg in messages {
        let body = msg.encode_body().unwrap();
        let back = Message::decode_known(msg.msg_type(), &body).unwrap();
        assert_eq!(back, Some(msg.clone()), "round trip failed for {msg:?}");
    }
}
