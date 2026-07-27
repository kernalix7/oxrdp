# Fuzzing the protocol decoder

`oxproto` parses untrusted input by design — the agent decodes whatever reaches its listening
socket — so the decoder is fuzzed.

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run message      # arbitrary message bodies, round-trip stability
cargo +nightly fuzz run reassembly   # arbitrary chunk streams, size-limit invariant
```

This directory is its own workspace because cargo-fuzz requires nightly while the main
workspace is pinned to stable.

A deterministic smoke version of the same properties runs on stable in CI:
`crates/oxproto/tests/robustness.rs`.
