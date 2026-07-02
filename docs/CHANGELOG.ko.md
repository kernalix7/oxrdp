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
- **M0 — Connect-Initial 토대.** BER(ASN.1) 한정 길이/boolean/tag-length 프리미티브(`ber`),
  그리고 GCC 클라이언트 user-data 블록 `ClientCoreData`(CS_CORE), `ClientSecurityData`
  (CS_SECURITY), `ClientNetworkData`(CS_NET) (MS-RDPBCGR 2.2.1.3.x). 이후 단계에서 MCS
  Connect-Initial PDU로 조립됨. `oxrdp-pdu` + `oxrdp-core` 합쳐 41개 테스트.
- **M0 — MCS DomainParameters + 서버 GCC 블록.** BER `DomainParameters` SEQUENCE
  (target/minimum/maximum 세트, 양수 부호 바이트 포함 최소 정수 인코딩)와, MCS Connect-Response에
  실리는 서버 측 GCC user-data 블록 `ServerCoreData`(SC_CORE)·`ServerNetworkData`(SC_NET).
  `oxrdp-pdu` + `oxrdp-core` 합쳐 50개 테스트.
- **M0 — MCS Connect-Initial / Connect-Response (Basic Settings Exchange).**
  `ConnectInitial::to_bytes()`가 GCC Conference Create Request(T.124 OID, `Duca` H.221
  클라이언트 키, target/minimum/maximum `DomainParameters`)를 감싼 BER MCS Connect-Initial을
  클라이언트 데이터 블록과 함께 빌드(MS-RDPBCGR 2.2.1.3). `ConnectResponse::from_bytes()`가
  서버 Connect-Response를 파싱해 `McDn` 서버 키 뒤의 서버 core/network 데이터(MCS 채널 ID)를
  추출. `oxrdp-pdu` + `oxrdp-core` 합쳐 56개 테스트.
- **M0 — 전체 연결 시퀀스 상태머신.** `oxrdp-core`의 `ClientConnector`가 이제 RDP 연결 시퀀스
  전체를 sans-io `step()` 머신으로 구동: X.224 협상 → TLS 업그레이드 신호 → MCS Connect-Initial
  → Connect-Response(서버 채널 ID 추출) → Erect Domain + Attach User → Channel Join 루프 →
  `Connected`. `oxrdp-pdu::frame`(TPKT + X.224 data 감싸기/벗기기) 추가. 전체 핸드셰이크 시뮬
  테스트로 connector를 끝까지 구동. 57개 테스트.
- **M0 — TLS 설정 + 비동기 프레이밍 (impure 셸 시작).** `oxrdp-crypto`가 협상 이후 TLS
  업그레이드용 rustls `ClientConfig`를 제공 — trust-on-first-use 인증서 검증자(`TofuVerifier`,
  FreeRDP `/cert:tofu` 자세). 기밀성은 보장하나 MITM 방어는 아님(피닝은 예정 강화). `oxrdp-io`에
  tokio 스트림 위 비동기 TPKT 프레임 코덱(`read_frame`/`write_frame`) 추가. 첫 외부 의존성:
  `rustls`(ring 프로바이더), `tokio`. 62개 테스트.
- **M0 — 연결 드라이버 + 실행 가능한 `oxrdp` 바이너리.** `oxrdp-io::connect()`가 전송 계층을
  끝까지 조립: TCP → X.224 협상 → TLS 업그레이드(`tokio-rustls`) → MCS Connect-Initial부터
  채널 조인까지, sans-io `ClientConnector`를 구동하고 `Session`(TLS 스트림 + 협상된 채널 ID)을
  반환. `oxrdp` CLI가 이제 실행 가능 — `oxrdp <host[:port]> [username]`이 핸드셰이크를 수행하고
  협상된 채널을 보고. connect seam은 실서버로 검증하며, 이후 단계(보안/라이선싱/능력, 그래픽,
  RAIL)는 아직 미구현.
- **M0 — Client Info PDU + 보안 헤더.** `oxrdp-pdu::client_info`가 RDP Client Info
  PDU(TS_INFO_PACKET, MS-RDPBCGR 2.2.1.11.1.1)를 빌드: 로그온 플래그, 도메인/사용자/비밀번호/
  대체셸/작업디렉터리(UTF-16LE), 확장 정보(클라이언트 주소, 172바이트 타임존, 세션 ID, 성능
  플래그) — 채널 조인 이후 보내는 자격증명. `security::SecurityHeader`는 이 MCS 페이로드를 감싸는
  Basic Security Header(`SEC_INFO_PKT`/`SEC_LICENSE_PKT` 플래그). 67개 테스트.
- **M0 — share 프레이밍 + 라이선싱.** `oxrdp-pdu::share`가 능력교환·데이터 PDU를 감싸는
  `ShareControlHeader`·`ShareDataHeader`(TS_SHARECONTROLHEADER / TS_SHAREDATAHEADER) 추가.
  `oxrdp-pdu::license`가 라이선싱 PDU를 파싱해 흔한 "valid client — 라이선스 없이 진행" 경로
  (ERROR_ALERT / STATUS_VALID_CLIENT)를 감지. 71개 테스트.
- **M0 — 능력 교환.** `oxrdp-pdu::caps`가 General / Bitmap / Input 능력 세트와
  `default_client_capabilities` 번들을 추가. `oxrdp-pdu::active`가 서버 Demand Active PDU를
  파싱(shareId 추출)하고 클라이언트 Confirm Active PDU(능력 세트 포함)를 빌드. (증분 — 완전한
  Windows 상호운용엔 능력 세트 추가 필요.) 78개 테스트.
- **M0 — 최종화 PDU.** `oxrdp-pdu::finalize`가 연결 최종화 데이터 PDU 본문을 추가: Client
  Synchronize, Control(cooperate / request-control), Font List. 연결 시퀀스 PDU 세트 완성 —
  connector의 post-connection 시퀀스(Client Info → 라이선싱 → 능력교환 → 최종화)에 배선하는
  것이 다음. 81개 테스트.

[Unreleased]: https://github.com/kernalix7/oxrdp/commits/main
