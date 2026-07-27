#![no_main]
//! Decode arbitrary bytes as every message type. The property: never panic, never hang.
//! A decoded message must also re-encode and decode back to the same value.

use libfuzzer_sys::fuzz_target;
use oxproto::Message;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let msg_type = data[0];
    let body = &data[1..];

    if let Ok(Some(msg)) = Message::decode_known(msg_type, body) {
        // Round-trip stability: what we accepted must survive an encode/decode cycle.
        let re = msg.encode_body().expect("a decoded message must re-encode");
        let back = Message::decode_known(msg_type, &re).expect("re-encoded message must decode");
        assert_eq!(back, Some(msg), "round trip changed the message");
    }
});
