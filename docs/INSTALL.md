# Installation

**English** | [한국어](INSTALL.ko.md)

> **Status: planned — not yet installable.** oxrdp is pre-alpha; there is no buildable
> client or release artifact yet. This document is a placeholder that will be filled in
> when the first usable milestone (see the [roadmap](ARCHITECTURE.md#5-milestone-roadmap))
> produces something you can install. It is intentionally empty of instructions rather
> than describing steps that do not work.

## For now (developers only)

To build the workspace as it comes up, see [CONTRIBUTING.md](../CONTRIBUTING.md). Once the
Cargo workspace exists:

```bash
git clone https://github.com/kernalix7/oxrdp.git
cd oxrdp
cargo build --workspace
```

## Planned distribution

Distribution channels (crates.io, distro packages, prebuilt binaries) will be decided and
documented here closer to v0. oxrdp is consumed by [winpodx](https://github.com/kernalix7/winpodx)
as a library + thin binary; the winpodx install flow will pull it in once it ships.
