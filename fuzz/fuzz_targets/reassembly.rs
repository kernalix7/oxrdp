#![no_main]
//! Feed arbitrary chunk streams to the reassembler. The property: never panic, and never
//! exceed the per-type size limit no matter how the peer fragments its input.

use libfuzzer_sys::fuzz_target;
use oxproto::envelope::{max_message_len, ChunkHeader, Reassembler};
use oxproto::{decode, CHUNK_HEADER_LEN};

fuzz_target!(|data: &[u8]| {
    let mut reassembler = Reassembler::new();
    let mut rest = data;

    while rest.len() >= CHUNK_HEADER_LEN {
        let Ok(header) = decode::<ChunkHeader>(&rest[..CHUNK_HEADER_LEN]) else {
            return;
        };
        rest = &rest[CHUNK_HEADER_LEN..];

        let len = header.length as usize;
        if rest.len() < len {
            return;
        }
        let (payload, tail) = rest.split_at(len);
        rest = tail;

        match reassembler.push(&header, payload) {
            Ok(Some(msg)) => {
                assert!(
                    msg.payload.len() <= max_message_len(msg.msg_type),
                    "reassembled message exceeded its type limit"
                );
            }
            Ok(None) => {}
            Err(_) => return,
        }
    }
});
