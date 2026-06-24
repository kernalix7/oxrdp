# Contributing to oxrdp

**English** | [한국어](docs/CONTRIBUTING.ko.md)

Thank you for your interest in contributing to oxrdp! This guide will help you get started.

> **Status: pre-alpha.** oxrdp is a from-scratch, memory-safe RDP client in Rust. The
> protocol stack is under active construction (see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
> for the workspace layout and the M0–M5 roadmap). Expect churn.

## Prerequisites

- Rust stable (latest; CI pins the current stable toolchain). MSRV is not yet fixed.
- For the display/render shells (later milestones): a Wayland and/or X11 dev environment,
  `libxkbcommon`, and a VA-API-capable GPU stack for hardware H.264 decode (`openh264`
  software decode is the portable fallback).

## Build

```bash
git clone https://github.com/kernalix7/oxrdp.git
cd oxrdp
cargo build --workspace
```

## Test

```bash
# Run tests (the sans-io core is testable without a server, via captured-trace replay)
cargo test --workspace

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Format check
cargo fmt --all -- --check
```

The pure core crates (`oxrdp-pdu`, `oxrdp-core`, `oxrdp-graphics`, `oxrdp-channels`,
`oxrdp-rail`) are IO-free and fully testable in CI. The shell crates (`oxrdp-io`,
`oxrdp-display`, `oxrdp-render`, `oxrdp-input`) touch the network, a windowing system,
and the GPU — changes that affect them must be exercised against a real Windows RDP
server (e.g. the winpodx dockur/windows guest) before merge.

## Workflow

1. **Fork** the repository
2. Create a **feature branch** (`git checkout -b feat/my-feature`)
3. Write your changes following **conventional commits**
4. Submit a **Pull Request**

## PR Checklist

Before submitting a PR, ensure the following:

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` reports zero warnings
- [ ] `cargo fmt --all -- --check` passes
- [ ] No `unsafe` without a `// SAFETY:` justification comment
- [ ] Documentation is updated (if applicable; docs are bilingual — ko & en)
- [ ] No hardcoded credentials or secrets

## Commit Convention

This project follows [Conventional Commits](https://www.conventionalcommits.org/):

| Prefix | Purpose |
|--------|---------|
| `feat` | New feature |
| `fix` | Bug fix |
| `docs` | Documentation changes |
| `refactor` | Code refactoring (no feature change) |
| `test` | Adding or updating tests |
| `chore` | Maintenance tasks (CI, deps, etc.) |

### Examples

```
feat: add Wayland display backend
fix: correct RAIL z-order on popup windows
docs: update architecture parity matrix
refactor: split H.264 decode into the render shell
test: add fuzz target for GFX PDU decode
chore: bump rustls to 0.23.x
```

### No AI tool co-author trailers

Do **not** add `Co-authored-by:` trailers that name AI tools / coding agents. This applies to all of:

- `Co-authored-by: Cursor <cursoragent@cursor.com>`
- `Co-authored-by: Claude <noreply@anthropic.com>` (and any other Anthropic email)
- `Co-authored-by: Copilot <...>` (any GitHub Copilot variant)
- `Co-authored-by: <any other AI tool / agent identity>`

You wrote the patch — the human author of record is you. AI tooling doesn't get co-authorship credit in this repo regardless of how much it contributed. If you forgot and a trailer slipped in, we'll ask you to amend.

Human co-authors (e.g., a colleague who pair-programmed with you on the change) are fine and welcome — those should use real human identities + emails.

## Writing release notes

Each version section in `CHANGELOG.md` (and `docs/CHANGELOG.ko.md`) starts with
`### Highlights` — a one-sentence headline followed by 3–6 scannable bullets, then the
detailed `### Added` / `### Changed` / `### Fixed` bullets underneath.

Skeleton:

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Highlights

**One-sentence headline.** Optional 1-2 sentence elaboration if needed.

- Most important user-visible change (one line, scannable)
- (3-6 bullets max; no prose blocks)

### Added
- (detailed bullets)

### Changed
- (detailed bullets)

### Fixed
- (detailed bullets)
```

### Crediting contributors in Highlights

When a Highlights bullet covers work that came from outside the maintainer (external PR
or external bug report / feature request), credit the contributor inline:

| Source | Suffix |
|---|---|
| External PR (someone else's commits) | `(by @username, #PR)` |
| External issue / feature request (maintainer wrote the code) | `(reported by @username, #issue)` |
| Both — external report **and** external PR by the same person | `(by @username, #PR / #issue)` |

The "no AI tool co-author trailers" rule above is unrelated: it bans machine-generated
attribution. Human contributors are credited liberally and explicitly.

## Security

If you discover a security vulnerability, please follow the process described in [SECURITY.md](SECURITY.md). **Do NOT open a public issue.**
