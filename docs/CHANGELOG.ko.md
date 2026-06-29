# 변경 이력(Changelog)

[English](../CHANGELOG.md) | **한국어**

oxrdp의 모든 주목할 만한 변경 사항이 여기에 기록됩니다. 형식은
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/)를 기반으로 하며, 프로젝트는
릴리스가 시작되면 [유의적 버전(Semantic Versioning)](https://semver.org/)을 따르는 것을
목표로 합니다.

## [Unreleased]

### Highlights

**프로젝트 부트스트랩.** oxrdp는 winpodx 뒤에서 동작하는 독립형, 처음부터 작성한 Rust RDP
엔진으로 분리되었으며, winpodx의 FreeRDP 경로와 드롭인 동등성(drop-in equivalence)을
이루는 것을 v0 목표로 합니다.

- 아키텍처 확정: sans-io 순수 프로토콜 코어 + 플러그형 IO / 디스플레이 / 렌더 / 입력 셸;
  하나의 `DisplayBackend` 트레이트 뒤의 X11 + Wayland.
- 렌더링 경로 확정: 시작부터 `wgpu` GPU, VA-API 하드웨어 H.264 디코드와 `openh264`
  소프트웨어 대체 수단(`wgpu`로의 DMA-BUF 무복사).
- 범위 확정: 단계적 프로토콜 표면; v0는 winpodx가 사용하는 정확한 FreeRDP 기능 집합과의
  동등성을 목표로 하며, NLA/CredSSP는 보류(winpodx는 `/sec:tls`를 사용).
- 프로젝트 구조, MIT 라이선스, 이중 언어(en/ko) 문서 확립.

### Added
- `README.md` 및 `docs/ARCHITECTURE.md` — 프로젝트 정체성, 확정된 결정, FreeRDP→oxrdp
  동등성 매트릭스, 크레이트 워크스페이스 구성, M0–M5 로드맵.
- 커뮤니티 헬스 파일(CODE_OF_CONDUCT, CONTRIBUTING, SECURITY, THIRD_PARTY_LICENSES),
  GitHub 이슈/PR 템플릿, Rust CI 워크플로.
- Cargo 워크스페이스 스캐폴드 — 12개 크레이트(`oxrdp-pdu`, `oxrdp-core`, `oxrdp-graphics`,
  `oxrdp-channels`, `oxrdp-rail`, `oxrdp-crypto`, `oxrdp-io`, `oxrdp-display`,
  `oxrdp-render`, `oxrdp-input`, `oxrdp` 파사드, `oxrdp-cli` 바이너리)를 빌드되는
  스켈레톤으로 추가. 순수 코어 크레이트는 `#![forbid(unsafe_code)]`. `cargo build/test/
  clippy/fmt` 모두 통과.
- **M0 — `oxrdp-pdu` 코덱 토대.** 손수 작성한 `Decode`/`Encode` 트레이트와, 변형/절단된
  서버 입력에 절대 panic하지 않는 bounds-checked `ReadCursor`/`WriteCursor`, 타입드
  `DecodeError`/`EncodeError`. 첫 프레이밍 PDU: `TpktHeader`(RFC 1006), `X224DataHeader`.
  외부 의존성 0. 단위 테스트 9개.
- **M0 — 연결 설정 PDU.** RDP 보안 협상(`NegotiationRequest`/`NegotiationResponse`/
  `NegotiationFailure`, MS-RDPBCGR 2.2.1.1.1/2.2.1.2.x)과, 협상 및 `mstshash` 라우팅 쿠키를
  실어 나르는 X.224 Connection Request/Confirm TPDU(`ConnectionRequest`/`ConnectionConfirm`).
  누적 단위 테스트 19개.
- **M0 — MCS 도메인 PDU.** PER 인코딩된 `ErectDomainRequest`, `AttachUserRequest` /
  `AttachUserConfirm`, `ChannelJoinRequest` / `ChannelJoinConfirm` (T.125 / MS-RDPBCGR
  2.2.1.3–2.2.1.8). MCS 사용자 채널 base-1001 initiator 오프셋 포함. 누적 단위 테스트 25개.
- **M0 — MCS Send Data 래퍼 + sans-io 연결 상태머신.** `SendDataRequest` /
  `SendDataIndication`(채널조인 이후 모든 PDU가 실리는 MCS 봉투, PER 길이 인코딩) — `oxrdp-pdu`.
  그리고 `oxrdp-core`의 `ClientConnector` — `step()` 구동 IO-free 상태머신으로 X.224 협상 단계를
  수행: TPKT로 감싼 Connection Request 방출, Connection Confirm 파싱, TLS 업그레이드와 선택된
  프로토콜 신호. 두 크레이트 합쳐 33개 테스트.

[Unreleased]: https://github.com/kernalix7/oxrdp/commits/main
