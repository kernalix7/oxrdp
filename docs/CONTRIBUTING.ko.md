# oxrdp에 기여하기

[English](../CONTRIBUTING.md) | **한국어**

oxrdp에 기여하는 데 관심을 가져 주셔서 감사합니다! 이 가이드가 시작하는 데 도움이 될 것입니다.

> **상태: 프리알파(pre-alpha).** oxrdp는 Rust로 처음부터 작성한 메모리 안전 RDP 클라이언트입니다.
> 프로토콜 스택은 활발히 구축되고 있습니다(워크스페이스 구성과 M0–M5 로드맵은
> [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) 참조). 잦은 변경을 예상하십시오.

## 사전 요구사항

- Rust stable(최신; CI는 현재 stable 툴체인을 고정합니다). MSRV는 아직 확정되지 않았습니다.
- 디스플레이/렌더 셸(이후 마일스톤)을 위해: Wayland 및/또는 X11 개발 환경,
  `libxkbcommon`, 그리고 하드웨어 H.264 디코드를 위한 VA-API 지원 GPU 스택(`openh264`
  소프트웨어 디코드가 이식 가능한 대체 수단입니다).

## 빌드

```bash
git clone https://github.com/kernalix7/oxrdp.git
cd oxrdp
cargo build --workspace
```

## 테스트

```bash
# 테스트 실행(sans-io 코어는 캡처된 트레이스 재생을 통해 서버 없이 테스트 가능)
cargo test --workspace

# 린트
cargo clippy --workspace --all-targets -- -D warnings

# 포맷 검사
cargo fmt --all -- --check
```

순수 코어 크레이트(`oxrdp-pdu`, `oxrdp-core`, `oxrdp-graphics`, `oxrdp-channels`,
`oxrdp-rail`)는 IO가 없으며 CI에서 완전히 테스트할 수 있습니다. 셸 크레이트(`oxrdp-io`,
`oxrdp-display`, `oxrdp-render`, `oxrdp-input`)는 네트워크, 윈도잉 시스템, GPU를 다루므로,
이들에 영향을 주는 변경은 병합 전에 실제 Windows RDP 서버(예: winpodx의 dockur/windows
게스트)를 대상으로 검증해야 합니다.

## 워크플로

1. 저장소를 **포크(Fork)**합니다
2. **기능 브랜치(feature branch)**를 생성합니다(`git checkout -b feat/my-feature`)
3. **컨벤셔널 커밋(conventional commits)**을 따라 변경 사항을 작성합니다
4. **풀 리퀘스트(Pull Request)**를 제출합니다

## PR 체크리스트

PR을 제출하기 전에 다음을 확인하십시오:

- [ ] `cargo test --workspace`가 통과함
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`가 경고를 0개 보고함
- [ ] `cargo fmt --all -- --check`가 통과함
- [ ] `// SAFETY:` 정당화 주석 없는 `unsafe`가 없음
- [ ] 문서가 갱신됨(해당하는 경우; 문서는 이중 언어 — ko & en)
- [ ] 하드코딩된 자격 증명이나 비밀이 없음

## 커밋 규약

이 프로젝트는 [Conventional Commits](https://www.conventionalcommits.org/)를 따릅니다:

| 접두사 | 목적 |
|--------|--------|
| `feat` | 새로운 기능 |
| `fix` | 버그 수정 |
| `docs` | 문서 변경 |
| `refactor` | 코드 리팩터링(기능 변경 없음) |
| `test` | 테스트 추가 또는 갱신 |
| `chore` | 유지보수 작업(CI, 의존성 등) |

### 예시

```
feat: add Wayland display backend
fix: correct RAIL z-order on popup windows
docs: update architecture parity matrix
refactor: split H.264 decode into the render shell
test: add fuzz target for GFX PDU decode
chore: bump rustls to 0.23.x
```

### AI 도구 공동 작성자 트레일러 금지

AI 도구/코딩 에이전트를 명시하는 `Co-authored-by:` 트레일러를 추가하지 **마십시오**. 이는 다음 모두에 적용됩니다:

- `Co-authored-by: Cursor <cursoragent@cursor.com>`
- `Co-authored-by: Claude <noreply@anthropic.com>`(및 그 밖의 모든 Anthropic 이메일)
- `Co-authored-by: Copilot <...>`(모든 GitHub Copilot 변형)
- `Co-authored-by: <그 밖의 모든 AI 도구/에이전트 신원>`

당신이 패치를 작성했으므로 — 기록상의 인간 작성자는 당신입니다. AI 도구가 아무리 많이 기여했더라도 이
저장소에서는 공동 저작권을 인정받지 못합니다. 잊고 트레일러가 섞여 들어갔다면, 수정(amend)을 요청할 것입니다.

인간 공동 작성자(예: 해당 변경 사항을 함께 페어 프로그래밍한 동료)는 괜찮으며 환영합니다 — 이런 경우에는
실제 인간의 신원 + 이메일을 사용해야 합니다.

## 릴리스 노트 작성

`CHANGELOG.md`(및 `docs/CHANGELOG.ko.md`)의 각 버전 섹션은
`### Highlights`로 시작합니다 — 한 문장의 헤드라인 다음에 훑어보기 좋은 3–6개의 불릿이 오고,
그 아래에 상세한 `### Added` / `### Changed` / `### Fixed` 불릿이 옵니다.

뼈대:

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Highlights

**한 문장 헤드라인.** 필요하다면 선택적으로 1-2 문장 부연 설명.

- 가장 중요한 사용자 가시적 변경 사항(한 줄, 훑어보기 좋게)
- (최대 3-6개의 불릿; 산문 블록 금지)

### Added
- (상세한 불릿)

### Changed
- (상세한 불릿)

### Fixed
- (상세한 불릿)
```

### Highlights에서 기여자 표기

Highlights 불릿이 메인테이너 외부에서 온 작업(외부 PR 또는 외부 버그 신고/기능 요청)을 다룰 때는,
인라인으로 기여자를 표기하십시오:

| 출처 | 접미사 |
|---|---|
| 외부 PR(타인의 커밋) | `(by @username, #PR)` |
| 외부 이슈/기능 요청(메인테이너가 코드 작성) | `(reported by @username, #issue)` |
| 둘 다 — 동일인의 외부 신고 **및** 외부 PR | `(by @username, #PR / #issue)` |

위의 "AI 도구 공동 작성자 트레일러 금지" 규칙은 이와 무관합니다: 그것은 기계가 생성한
표기를 금지하는 것입니다. 인간 기여자는 후하고 명시적으로 표기됩니다.

## 보안

보안 취약점을 발견한 경우, [SECURITY.md](SECURITY.md)에 설명된 절차를 따라 주십시오. **공개 이슈를 열지 마십시오.**
