# 변경 이력(Changelog)

[English](../CHANGELOG.md) | **한국어**

oxrdp의 모든 주목할 만한 변경 사항이 여기에 기록됩니다. 형식은
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/)를 기반으로 하며, 프로젝트는
릴리스가 시작되면 [유의적 버전(Semantic Versioning)](https://semver.org/)을 따르는 것을
목표로 합니다.

## [Unreleased]

### 보안 (2026-07-28)

에이전트의 네트워크 노출 표면에 대한 대항적 검토, 입력 주입 랜딩에 자극받아 — 그 전에는 인증
약점이 픽셀을 새었고, 이제는 공격자에게 인터랙티브 Administrator 세션에서 합성 키입력을 쥐여준다.

수정:

- **에이전트의 TLS 비밀키가 전 세계 읽기 가능으로 쓰였음**(기본 umask 하 0644). 모든 로컬
  계정이 읽을 수 있어, 그 pin을 신뢰하는 어떤 클라이언트에든 에이전트를 가장할 수 있었다: 피닝은
  공개키 해시를 검증하고 TLS는 매칭 비밀키 소유를 증명. TLS 1.3의 forward secrecy는 이것이 결코
  예전 트래픽 복호 버그가 아니었음을 의미 — '이제부터 우리를 가장'하는 버그였다. 키는 이제 0600으로
  생성. Windows ACL 강화는 여전히 간극이며 묵시적으로 두지 않고 호출 지점에 문서화.
- **`verify_token`이 인증되지 않은 상대가 보내기로 선택한 길이를 순회** — 서버의 고정 토큰 길이가
  아니라, 그래서 in-bounds 인덱스 검사 수가 공격자 입력에 따라 달라졌다 — 좁지만, 함수 자체의
  문서화된 계약 위반. 이제 항상 예상 길이로 실행.

또한 수정, 실 게스트로 검증:

- **미인증 서비스 거부.** accept 루프가 크레이트 어디에도 타임아웃 없이 순차 핸드셰이크·서브,
  그래서 아무것도 보내지 않는 TCP 커넥션 하나가 `accept()`를 무한 블로킹하고 오퍼레이터를
  잠가버렸다. 커넥션은 이제 단일 인증 전 마감시간 아래 자체 태스크를 받으며, 그 단계의 커넥션 수는
  상한, 그리고 인증된 세션은 한 번에 하나 보존. 게스트에서 측정: silent peer는 이제 20s 후 닫히고,
  정당한 TLS 핸드셰이크는 silent peer 세 개가 잡혀 있는 동안 즉시 완료 — 이전엔 아예 완료되지
  않았다.
- 커넥션별 경로의 panic 하나가 전체 프로세스를 끌어냈다 — 세션이 spawned가 아니라 직접
  await됐기 때문. 커넥션별 태스크가 이를 한 세션으로 묶었다.

그 태스크들은 `tokio::spawn`이 아니라 `LocalSet`/`spawn_local`을 쓰는데, 이것이 깨진 채 출고될
뻔했던 디테일: WinRT와 D3D11 인터페이스는 `!Send`이고, 호스트 빌드는 모듈이 `cfg(windows)`라 절대
마주치지 않는다 — 오직 Windows 크로스컴파일만 잡아낸다. 수정은 그 태스크를 COM 객체를 이미 소유한
스레드에 머물게 하는 것이지, `Send`를 단언하는 게 아니다.

검토하고 건전함 확인: pin은 어떤 성공 반환 전에도 검사되며 서명 검증자는 pinned 인증서에 위임;
인증 전 재조립 상한은 `Reassembler::push`를 통하는 모든 경로(channel spreading과
completion-and-reuse 포함)에서 유지; 그리고 디코드 체인 어디에서도 길이 검증 앞에 할당이
일어나지 않는다.

**기록 정정:** commit `2d155a5`는 메시지가 H.264 디코더를 기술하는데, 위 `oxsec` 수정 두 개도
함께 담고 있다. 이들은 여러 변경이 트리에 동시에 있던 중에 광범위 `git add`로 부지중
스테이징됐다. 히스토리의 코드는 정확하고 완전; 그 커밋 메시지만이 담은 것에 대해 오도적.

### 첫 엔드투엔드 구동 (2026-07-28)

이제 게스트에서 Windows 앱 창을 캡처해 **네이티브 Linux 창**으로 실시간 표시. 전체 경로를
oxrdp 자체 dockur 게스트로 실제 구동: WGC 캡처 → `oxproto` 프레이밍 → SPKI 피닝과 토큰 인증을
갖춘 TLS → `oxclient` → `oxdisplay`(winit + softbuffer). 1115×628 RAW_BGRA 를 ~21 fps로 측정,
단일 창에서 약 470 Mbit/s — P5 H.264 인코더를 위한 구체적 사례.

실제 구동으로만 발견한 버그 셋, 테스트 스위트가 잡을 수 없었던 것들:

- **WGC 픽셀 포맷.** 프레임풀이 `B8G8R8A8UIntNormalizedSrgb`로 생성됨.
  `Direct3D11CaptureFramePool`은 `B8G8R8A8UIntNormalized`와 `R16G16B16A16Float`만 받아들이고
  그 외는 bare `E_INVALIDARG`로 거부.
- **빈 풀 센티널.** `TryGetNextFrame`이 빈 풀을 `S_OK`를 담은 `Err`로 보고. 이를 실패로
  취급해 호출자가 매 틱마다 캡처를 재구축, 풀이 채워질 만큼 살지 못해 스트림은 바쁜 척하면서
  프레임을 0개 생산.
- **취소 안전성.** `read_reassembled`이 읽기 진행 상태를 future에 보관, 청크 도중에
  드롭된 `tokio::select!` 분기는 소비한 바이트를 잃고 스트림이 페이로드 도중에 재개. 진행
  상태를 호출자 상태에 보관하는 `ChunkReader` 추가; `ClientSession`은 이를 통해 읽고
  재개 가능하게 쓰기를 버퍼링.

`dev/vm/oxrdp-windows.sh status`도 재작성: 예전 프로브는 건강한 에이전트를 "NOT running"으로
보고(rustls이 절단된 핸드셰이크에 alert로 응답하지 않기 때문). 이제 진짜 TLS 핸드셰이크를
완료하고 SPKI pin 출력, 이를 에이전트 자체 `--print-pin`과 대조해 확인.

### 방향 전환 (2026-07-02)

oxrdp가 "더 나은 RDP **클라이언트**"에서 **RDP 자체를 대체**하는 목적형 저지연 remote-app
프로토콜(RustDesk/Moonlight식)로 전환. Windows 게스트 **에이전트**가 개별 앱 창을 캡처해
Linux **클라이언트**로 커스텀 프로토콜(QUIC, TCP 폴백)로 스트리밍. 근거: RDP는 구조적 지연
한계(기본 TCP head-of-line 블로킹, 대역폭 최적화 버퍼링, 범용 오버헤드)가 있어 목적형
프로토콜이 이길 수 있음. 실 Windows로 MCS 채널조인까지 검증한 기존 RDP 클라이언트 작업은
git 히스토리에 남기고 **셸빙**; 클라 셸(TLS·전송·wgpu 디코드·창매핑·입력)과 코덱 토대는 재활용.
새 에이전트: Rust+windows-rs · Windows.Graphics.Capture · 런타임 HW/SW 인코드 · QUIC+TCP.

- **P0 — `oxproto`.** 새 프로토콜의 sans-io wire 메시지: `Message` envelope(ClientHello /
  ServerHello / WindowCreated / WindowClosed / FrameData / PointerEvent), oxrdp-pdu 코덱 재활용. 7 테스트.
- **P1 준비 — 크로스컴파일 파이프라인 + `oxagent` 골격.** Windows 게스트 에이전트를 리눅스에서
  `x86_64-pc-windows-gnu`(mingw-w64)로 크로스컴파일 — `oxagent.exe`가 windows-rs 0.58(WGC +
  Media Foundation + Win32 창열거) 링크. Windows deps는 `cfg(windows)` 게이트라 리눅스에선
  스텁으로 빌드되어 CI green 유지 — 에이전트를 게스트 내 툴체인 없이 리눅스에서 개발·빌드.
- **갭 감사 + 하드닝.** 다중 에이전트 감사(반증 검증된 56건, `docs/design/AUDIT-2026-07.md`)
  결과: Windows 에이전트를 크로스컴파일·린트하는 CI 잡, 차단형 `cargo audit`과 라이선스/금지/소스
  검사를 위한 `cargo deny`, 툴체인 핀, 그리고 문서가 검증 없이 단언하던 주장 정정(VA-API → wgpu
  DMA-BUF 임포트를 "미검증"으로 표기).
- **P1c — WGC 창별 캡처.** `oxagent`가 D3D11 디바이스·free-threaded 프레임풀·재사용 스테이징
  텍스처로 창 하나를 BGRA로 캡처(row-pitch 인식 리드백, 리사이즈 시 프레임풀 재생성). 창 열거는
  cloaked/tool/child/shell 창을 걸러내고 DWM 확장 프레임 경계를 보고하며, 프로세스는 per-monitor
  DPI 인식.
- **oxproto v1 — 프로토콜 재설계.** `docs/design/OXPROTO.md`에 명세하고 구현: 단편화와 채널별
  재조립을 갖춘 8바이트 청크 envelope(키프레임이 입력·제어를 head-of-line 블로킹할 수 없음),
  권위적 길이, 할당 전에 강제되는 타입별 크기 제한, 버전 범위와 기능 협상을 포함한 인증된
  핸드셰이크, 그리고 최초 설계에 없던 메시지 세트 — 키보드/텍스트/모디파이어 입력, 양방향 창 제어,
  커서 스트리밍, 프레임 ack와 품질 힌트, 지연 측정을 위한 단계별 타임스탬프, 분수 스케일링을 갖춘
  디스플레이 레이아웃, 앱 아이덴티티와 아이콘, 에러·종료·ping/pong. 알 수 없는 메시지 타입은
  치명적 오류가 아니라 건너뜀.
- **P2a — 클라이언트 세션.** `oxclient`가 핸드셰이크를 수행하고 기능을 협상하며 ping/pong을 투명하게
  처리하고 `ClientEvent` 스트림을 제공.
- **견고성.** `oxproto`에 결정적 스모크 퍼즈 테스트(임의 본문·청크 헤더가 절대 panic하지 않음, 절단은
  항상 에러, 선언된 길이가 수신자 할당을 유발할 수 없음)와 `fuzz/`의 cargo-fuzz 타깃 추가.
  `SECURITY.md`를 피벗 이후 뒤집힌 위협 모델(에이전트가 화면을 공유하고 입력을 주입하는 서버)로
  재작성했고, `docs/design/agent-runtime.md`가 게스트 세션·배포 모델을 확정. 122개 테스트.
- **P1 — 창 열거 + 비동기 전송.** `oxagent`가 보이는 최상위 창을 열거(`EnumWindows` →
  핸들/제목/좌표), windows-gnu 크로스컴파일 검증. `oxtransport`가 tokio 스트림 위로 oxproto
  메시지를 프레이밍(`read_message_bytes`/`write_message`, 64 MiB 가드). `oxproto`가
  `decode`/`encode_vec` 코덱 진입점을 re-export. 90 테스트.
- **`oxsec` — 에이전트 링크용 TLS.** 최초 실행 시 생성해 디스크에 저장하는 자체서명 에이전트
  identity, 호스트네임 검증 대신 클라이언트가 쓰는 SPKI-pin `ServerCertVerifier`(pin이 상대를
  인증하며 이름을 인증하는 게 아님), 핸드셰이크용 상수 시간 토큰 비교. 어떤 인증서든 받아들이는
  기존 `oxrdp-crypto::TofuVerifier`는 화면을 공유하고 입력을 주입하는 서버를 인증하기엔 부적합해
  재사용하지 않음. 7개 테스트.
- **P1d — 에이전트 세션 드라이버.** `oxagent`에 key/value 설정 로더(와일드카드 바인드 주소는
  기본값으로 피하는 게 아니라 아예 거부), 인증 전에는 메시지 정확히 하나만 받아들이는 인증
  게이트 핸드셰이크, 가장 오래된 미확인 프레임을 대기시키지 않고 드롭하는 창별 프레임 페이싱
  버짓(대역폭 저하를 무한 지연으로 바꾸는 큐잉이야말로 이 프로젝트가 피하려는 실패), 세션 동안
  프로토콜 id가 재사용되지 않는 창 레지스트리(OS는 네이티브 핸들을 재사용하는데, 재사용된 id는
  잘못된 네이티브 창에 새 픽셀을 그리게 됨), 그리고 이를 엮는 드라이버 `serve.rs`(핸드셰이크·창
  라이프사이클 diff·페이싱·ack 처리) 추가. 플랫폼은 `WindowSource` 트레이트 뒤에 있어 이 전부가
  리눅스 빌드 호스트에서 유닛 테스트됨 — 트레이트 구현만 Windows 전용. 리뷰 결과 새로 들어온
  코드를 추가 하드닝: envelope의 예약 플래그 비트는 이제 거부 대신 무시(전방 호환성), 인증 전에
  할당되는 재조립 상태는 이제 채널 64개·64 MiB로 상한을 둬 인증 전 메모리 증폭 경로를 차단.
  33개 테스트.
- **클라이언트 세션·창 모델·CLI.** `oxclient`에 원시 `ClientEvent` 스트림을 디스플레이 백엔드가
  실행할 순서 있는 명령 목록 — 이 네이티브 창 생성, 제목 변경, 재스택 — 으로 바꾸는 `WindowModel`
  추가. 각 백엔드가 프로토콜 메시지를 직접 diff할 필요가 없어짐; 프레임은 크고 비디오 속도로
  도착하므로 프레임 픽셀은 의도적으로 보관하지 않음. 새 `oxclient` 바이너리는 브링업 CLI —
  pinned TLS로 에이전트에 연결해 핸드셰이크를 수행하고 이벤트 스트림을 출력하며, 에이전트의
  페이싱 버짓이 전진하도록 프레임을 ack함. 토큰은 파일에서만 읽음 — `--token`을 커맨드라인에
  주면 거부(argv는 같은 사용자로 도는 다른 프로세스가 읽을 수 있음). 179개 테스트.
- **클라이언트 디스플레이/렌더 아키텍처 확정.** `docs/design/client-display.md`가 리눅스
  클라이언트의 윈도잉·프레젠테이션 스택을 확정: `winit`과 `x11rb` property 사이드카가 네이티브
  창을 영구적으로 소유, `softbuffer` 기반 CPU presenter가 첫 픽셀(P2b)에서
  `FrameData(RAW_BGRA)`를 blit — `wgpu`도 GPU 코드도 없음 — H.264 마일스톤(P5)에서만 새
  `oxrender` 크레이트의 `wgpu` presenter가 들어옴. `docs/ARCHITECTURE.md` §3의 `DisplayBackend`
  스케치와 `docs/HANDOFF.md`가 전에 담고 있던 "FrameData → wgpu texture" 표현을 대체.
  `oxrdp-display`·`oxrdp-render`·`oxrdp-input`은 삭제 대상으로 표시(내용을 채우지 않음).

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
- **M0 — 실 Windows 대상 첫 실접속 핸드셰이크. ✅** 동작 중인 Windows RDP 서버로 `oxrdp-cli`
  검증: 전체 연결 시퀀스(X.224 협상 → TLS → MCS Connect-Initial/Response → Erect Domain →
  Attach User → 채널조인 루프)가 완료되고 협상된 MCS 채널에 도달. BER/GCC/MCS/DomainParameters
  바이트 인코딩이 실 Windows에 대해 정확함을 입증. 검증으로 드러난 수정: CS_CORE에 모던 Windows가
  요구하는 **확장 필드**(`highColorDepth`/`supportedColorDepths`/`earlyCapabilityFlags`,
  216바이트) 추가 — 8bpp-only 미니멀 코어는 조용히 드롭됨. connect 드라이버 로깅은 `OXRDP_DEBUG`
  뒤로.

[Unreleased]: https://github.com/kernalix7/oxrdp/commits/main
