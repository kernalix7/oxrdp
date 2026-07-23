# dev/ — developer tooling

## `oxgen.sh` — offload code-gen to local ollama cloud models

Keeps Claude/Anthropic usage low by running the heavy per-file implementation on ollama cloud
models while the human authors the precise spec + `cargo` gates the result. See
[`../docs/HANDOFF.md` §7](../docs/HANDOFF.md) for the full workflow.

```bash
ollama serve &                                                  # if not already running
dev/oxgen.sh kimi-k2.7-code:cloud spec.txt crates/x/src/foo.rs  # think defaults to false
```

Token usage is appended to `.cloud-usage.tsv` next to the output file (these cloud models are a
small plan — watch it).

### Spec format that has worked well

A spec file is a single prompt that tells the model to emit **one complete Rust file, no prose,
no fences**, and includes:

1. The crate + file name and the lint/style bar (`#![forbid(unsafe_code)]` where applicable,
   `cargo clippy -- -D warnings`, `cargo fmt --check`).
2. The **exact existing API** the file must use (trait signatures, cursor methods with their
   `ctx: &'static str` args, the real error-enum variant shapes — models hallucinate these).
3. The precise wire layout / field order (little-endian, byte offsets).
4. **Authoritative test vectors** as an exact `#[cfg(test)] mod tests` block the model must
   include and pass — byte-exact where interop correctness matters.

Then gate with `cargo` and fix the model's misses (typically: wrong error variants, missing
`ctx` args, `Decode<'de>` lifetime, `ok_or_else`→`ok_or`).

**Do not** offload intricate `unsafe` Windows COM/WGC/Media-Foundation code — author it directly
(the windows-rs API is version-sensitive and models hallucinate it); validate by cross-compile.
