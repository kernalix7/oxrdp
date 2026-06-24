# oxrdp — 아키텍처

[English](ARCHITECTURE.md) | **한국어**

이 문서는 oxrdp의 엔지니어링 형태를 기록합니다: sans-io 코어, 크레이트 워크스페이스,
디스플레이 백엔드 추상화, v0의 "드롭인 동등성" 기준을 정의하는 FreeRDP 동등성 매트릭스,
그리고 마일스톤 로드맵.

이는 프로젝트 구조에 대한 진실의 원천(source of truth)입니다. 상위 수준의 근거는
[`../README.md`](../README.md)에 있습니다.

---

## 1. 설계 원칙: sans-io 코어, 비순수 셸

프로토콜 로직은 소켓, 시계, 윈도를 절대 건드리지 않습니다. 이는 바이트/이벤트를 소비하고
*출력*(전송할 바이트, 표면 갱신, 윈도 수명 주기 이벤트)을 방출하는 **순수 상태 머신**의
집합입니다. 모든 IO, 타이밍, 렌더링, 입력 캡처는 코어를 구동하는 얇은 "셸" 크레이트에
존재합니다.

```
            ┌──────────────────────── impure shells ────────────────────────┐
  network → │  oxrdp-io (tokio)  ──bytes──►  ┌─────────────────────────────┐  │
            │                                │        sans-io core         │  │
  display ← │  oxrdp-display     ◄─surfaces──┤  pdu · core · graphics ·    │  │
  (X11/Wl)  │  (X11 + Wayland)   ──events──► │  rail · channels            │  │ ← pure, no IO
            │                                └─────────────────────────────┘  │
  input   → │  oxrdp-input       ──input───►                                   │
            └────────────────────────────────────────────────────────────────┘
```

여기서 이것이 특별히 중요한 이유:
- **하나의 코어로 X11 + Wayland.** 백엔드 추상화는 윈도잉 시스템이 등장하는 *유일한*
  곳입니다; 프로토콜 코드는 둘 모두에 동일합니다.
- **테스트 가능성 및 퍼징.** 코어는 `feed(bytes) -> Vec<Output>` 함수입니다 —
  결정적이고, 캡처된 RDP 트레이스로부터 재생 가능하며, 서버 없이 퍼징 가능합니다.
- **데이터로서의 RAIL 정확성.** 원격 윈도 상태(목록, z-순서, 아이콘, 기하 구조,
  부모/소유자, 팝업 대 toplevel)는 `oxrdp-rail`에서 명시적으로 모델링되어 격리된 상태에서
  검증되며, 렌더 부수 효과에 암묵적으로 의존하지 않습니다 — 이 프로젝트를 촉발한 FreeRDP
  RAIL 버그를 직접 겨냥합니다.

## 2. 크레이트 워크스페이스

Cargo 워크스페이스. 순수 크레이트는 `tokio` / 윈도잉 의존성이 없습니다.

| 크레이트 | 순수성 | 책임 |
| --- | --- | --- |
| `oxrdp-pdu` | 순수 | 와이어 타입: 우리가 사용하는 모든 PDU의 인코딩/디코딩. 프로토콜 어휘. |
| `oxrdp-core` | 순수 | 연결 상태 머신: X.224, MCS, 기능 교환, 채널 조인, 연결 시퀀스. |
| `oxrdp-graphics` | 순수 | GFX/RFX/비트맵 **프로토콜** + 표면/영역 모델. 코덱 태그가 붙은 디코드 명령(`decode H.264/RFX/bitmap payload → surface S @ region R`)을 방출합니다; 하드웨어 디코더를 직접 실행하지 **않습니다** — 실제 픽셀 디코드는 렌더 셸에 존재합니다. |
| `oxrdp-render` | 셸 | H.264 디코드(**VA-API** HW, `openh264` SW 대체), RFX/비트맵 CPU 디코드, 그리고 `wgpu` 합성/표시. VA-API 프레임은 DMA-BUF를 통해 `wgpu`로 임포트됩니다(무복사). |
| `oxrdp-channels` | 순수 | 정적/동적 가상 채널: cliprdr(클립보드), rdpsnd(오디오 출력), rdpdr(드라이브/프린터), audin(마이크). |
| `oxrdp-rail` | 순수 | RAIL / RemoteApp: 원격 윈도 목록, 순서, 아이콘, 이동/크기 조정/최소화, 팝업, 언어 표시줄, 시스템 메뉴. "매끄러움(seamless)"의 심장. |
| `oxrdp-crypto` | 얇음 | 보안 글루: `rustls` TLS, 그리고(보류) `sspi-rs` NLA/CredSSP. `oxrdp-io`와 코어 사이에 위치합니다. |
| `oxrdp-io` | 셸 | `tokio` 전송: TCP, TLS 스트림, 프레이밍, sans-io 코어를 펌핑하고 그 출력을 플러시하는 비동기 드라이버. |
| `oxrdp-display` | 셸 | `DisplayBackend` 트레이트 + `x11` 및 `wayland` 백엔드. 원격 RAIL 윈도당 하나의 네이티브 toplevel. |
| `oxrdp-input` | 셸 | 키보드/마우스/터치 캡처 → 입력 PDU; 키보드 그랩 의미론; 스캔코드/키맵 변환. |
| `oxrdp` | 라이브러리 | 코어 + 셸을 연결하는 상위 수준 `Session` API. **이것이 winpodx가 링크하는 것입니다.** |
| `oxrdp-cli` | 바이너리 | 얇은 바이너리: 플래그 파싱, 구성, `oxrdp`를 선택된 백엔드에 연결. |

의존성 방향: 셸 → `oxrdp`(라이브러리) → 순수 코어 크레이트. 순수 크레이트는 절대 셸에
의존하지 않습니다.

## 3. 디스플레이 백엔드 추상화

책임의 분리: `oxrdp-display`는 **네이티브 윈도 수명 주기, 메타데이터, 입력**을 소유하며,
각 윈도에 `raw-window-handle`을 넘겨 `oxrdp-render`가 `wgpu` 표면을 거기에 바인딩합니다.
`oxrdp-render`는 `wgpu` 디바이스, 코덱 디코드, 그리기를 소유합니다. 따라서 아래의 `present`는
"디코드된 영역이 준비됨"을 의미합니다 — 실제 GPU 그리기는 렌더 셸에서 윈도의 `wgpu` 표면을
대상으로 일어납니다.

```rust
/// One implementation per windowing system (X11, Wayland).
trait DisplayBackend {
    /// A remote RAIL window appeared — create a native toplevel.
    fn create_window(&mut self, id: RemoteWindowId, attrs: &WindowAttrs) -> Result<()>;
    /// Blit a decoded surface region into a window.
    fn present(&mut self, id: RemoteWindowId, region: &SurfaceRegion) -> Result<()>;
    /// Title / WM_CLASS / icon / min-max / parent — the metadata that makes it feel native.
    fn set_metadata(&mut self, id: RemoteWindowId, meta: &WindowMeta) -> Result<()>;
    fn destroy_window(&mut self, id: RemoteWindowId) -> Result<()>;
    /// Pump native events (resize, move, focus, close, input) back toward the core.
    fn poll_events(&mut self) -> Vec<BackendEvent>;
}
```

- **X11 백엔드**(`x11rb`): 원격 윈도당 override-redirect / 일반 toplevel;
  `WM_CLASS`, `_NET_WM_*` 힌트, `_NET_WM_ICON`; winpodx의
  `MonitorDefArray`/`/multimon` 레이아웃 관련 사항이 여기에 존재합니다.
- **Wayland 백엔드**(`smithay-client-toolkit`): 원격 윈도당 `xdg_toplevel`.
  모델 제약에 주의 — Wayland 클라이언트는 절대 윈도 위치를 설정할 수 없으므로,
  RAIL 기하 구조 의미론이 X11과 다르며 코어가 아니라 여기서 처리됩니다.

## 4. FreeRDP → oxrdp 동등성 매트릭스(v0 기준)

winpodx가 `winpodx/core/rdp.py`에서 방출하는 정확한 `xfreerdp3` 플래그에서 도출되었습니다.
"v0" = 드롭인 동등성에 필요함; "Staged" = 단계적 프로토콜 표면 결정에 따라 보류됨.

| FreeRDP 플래그(winpodx) | 기능 | oxrdp 구성 요소 | v0? |
| --- | --- | --- | --- |
| `/v /u /d /p` | 연결 + 로그온 | `oxrdp-core` | **v0** |
| `/sec:tls`, `/cert:ignore\|tofu` | TLS 보안 + TOFU 인증서 | `oxrdp-crypto` | **v0** |
| *(NLA / CredSSP)* | 네트워크 수준 인증 | `oxrdp-crypto`(`sspi-rs`) | Staged — winpodx는 `/sec:tls`로 회피 |
| `/app:program,name,cmd`, `/app-name`, `/app-cmd` | RAIL / RemoteApp 실행 | `oxrdp-rail` | **v0** |
| `/wm-class` | 네이티브 윈도의 `WM_CLASS` | `oxrdp-display` | **v0** |
| `+grab-keyboard` | 키보드 그랩 | `oxrdp-input` | **v0** |
| `/gfx` (`h264`, `progressive`, `thin-client`, `small-cache`, `RFX`) | GFX 그래픽 파이프라인 | `oxrdp-graphics` | **v0**(H.264 AVC420/444) |
| `/rfx` | RemoteFX 대체 | `oxrdp-graphics` | **v0** |
| `/compression`, `/network`, `/codec`, `/bpp` | 성능 / 코덱 튜닝 | `oxrdp-core` + `oxrdp-graphics` | **v0** |
| 클립보드(cliprdr, FreeRDP 기본값) | 클립보드 동기화(양방향) | `oxrdp-channels` | **v0** |
| `/sound:sys:alsa` | 오디오 출력(rdpsnd) | `oxrdp-channels` | **v0** |
| `/drive:home`, `/drive:media,<base>` | 파일시스템 리디렉션(`\\tsclient`) | `oxrdp-channels`(rdpdr) | **v0** |
| `/multimon`, `/span`, `/smart-sizing`, `/size`, `/monitors` | 멀티 모니터 레이아웃 | `oxrdp-display` + `oxrdp-core` | **v0**(RAIL-primary + span) |
| `/scale`, `/scale-desktop`, `/scale-device` | HiDPI 스케일링 | `oxrdp-display` | **v0** |
| `/dynamic-resolution` | 동적 크기 조정(데스크톱 모드) | `oxrdp-channels`(dynvc) | **v0**(전체 데스크톱 모드) |
| `/window-position` | 초기 윈도 위치 | `oxrdp-display` | 있으면 좋음 |
| `/microphone` | 오디오 입력(audin) | `oxrdp-channels` | Staged |
| `/printer` | 프린터 리디렉션 | `oxrdp-channels`(rdpdr) | Staged |
| `/usb:auto` | USB 리디렉션 | `oxrdp-channels` | Staged |
| `/smartcard`, `/serial`, `/parallel` | 장치 리디렉션 | `oxrdp-channels` | Staged |
| `/gdi:sw\|hw` | GDI 재그리기 모드 | — | 해당 없음(자체 렌더러) |

### 동등성 작업이 존중해야 할 winpodx 고유 특이점

- **결합된 `/app:program:X,name:Y,cmd:Z` 구문** — FreeRDP 3의 RAIL 파싱은 쉼표로
  분리합니다; oxrdp의 `Session` API는 이것들을 구조화된 필드로 받으며, winpodx 어댑터가
  그것들에 매핑합니다. 우리 쪽에서는 셸 문자열 파싱이 필요 없습니다.
- **RAIL에 대한 `/span` 대 `/multimon`** — FreeRDP RAIL은 연속되지 않은 모니터
  레이아웃을 span할 수 없습니다; winpodx는 레이아웃이 타일링되지 않을 때 `/span|/multimon`
  없이 재시도합니다. oxrdp는 호스트 모니터 레이아웃을 명시적으로 모델링하고 RAIL 윈도를
  기본 모니터에 고정하여 `MonitorDefArray` 실패 모드를 회피합니다.
- **XWayland 하의 GFX** — winpodx는 XWayland GFX 표면 매핑 버그에 대해 `/gfx:RFX`
  대체 수단(RemoteFX 강제, H.264 건너뛰기)을 갖고 있습니다. oxrdp의 자체 렌더러는
  XWayland 표면 의존성을 제거하지만, RFX 경로는 어쨌든 협상된 대체 수단으로 유지됩니다.

## 5. 마일스톤 로드맵

각 마일스톤이 독립적으로 시연 가능하도록 순서를 정했습니다. v0의 성공 기준은 드롭인
동등성이므로 의도적으로 크지만, 하위 마일스톤이 이를 다룰 수 있게 만듭니다.

- **M0 — 스캐폴드 & 핸드셰이크.** 워크스페이스, sans-io 테스트 하니스, 캡처된
  RDP 트레이스 재생. `oxrdp-core`가 `/sec:tls`를 통해 dockur 게스트를 대상으로
  연결 시퀀스 / 기능 교환 단계에 도달합니다. 아직 픽셀 없음.
- **M1 — 첫 픽셀(데스크톱).** GFX H.264 + RFX 디코드; 전체 데스크톱 세션이 렌더링되고
  하나의 백엔드(X11 우선)를 통해 키보드/마우스 입력을 받습니다. IO ↔ 코어
  ↔ 디스플레이 ↔ 입력 루프를 끝에서 끝까지 증명합니다.
- **M2 — 첫 RAIL 윈도.** `oxrdp-rail`이 단일 RemoteApp 윈도를 올바른 `WM_CLASS`,
  제목, 아이콘, 입력을 갖춘 네이티브 toplevel에 매핑합니다. winpodx가 존재하는 바로 그
  이유인 수직 슬라이스.
- **M3 — RAIL 멀티앱 & 채널.** 올바른 z-순서/팝업을 갖춘 여러 개의 동시 RemoteApp 윈도;
  클립보드, 오디오 출력, `\\tsclient` 드라이브 리디렉션.
- **M4 — 디스플레이 동등성.** 멀티 모니터(RAIL-primary + span), HiDPI 스케일링, 동적
  해상도. Wayland 백엔드가 X11과 동등성에 도달합니다.
- **M5 — 드롭인 동등성(v0).** winpodx가 `oxrdp` 라이브러리를 통해 oxrdp에서 RAIL 멀티앱
  워크플로를 FreeRDP 경로와 동등하게 실행합니다. **v0 배포.**
- **v0 이후(단계적 표면).** NLA/CredSSP(`sspi-rs`), 마이크, 프린터, USB 및 기타 장치
  리디렉션, 그리고 임의의 RDP 서버 호환성으로의 확장.

## 6. winpodx 통합 형태

통합은 **라이브러리 + 얇은 바이너리**이지만, v0 *기준*은 드롭인 동등성입니다.
조화: `oxrdp`는 구조화된 `Session` API를 노출합니다; winpodx의 `core/rdp.py`는
`oxrdp` 세션 구성(오늘날 `xfreerdp3` 플래그로 인코딩하는 것과 동일한 기능)을 빌드하고
라이브러리를 링크하거나 `oxrdp-cli`를 호출하는 작은 어댑터를 갖게 됩니다. winpodx는
FreeRDP 스타일 플래그 문자열을 계속 방출할 필요가 **없습니다** — 동등성에 도달해야 하는
것은 CLI 구문이 아니라 기능입니다.

## 7. 해결된 결정(라운드 3)

- **렌더러: 시작부터 GPU(`wgpu`).** 윈도 합성, 스케일링, 표시가 윈도별 CPU 블릿이 아니라
  `wgpu`를 통합니다. 성능 한계를 끌어올리고 아래의 VA-API 디코드와 짝을 이룹니다.
- **H.264 GFX 디코드: VA-API 하드웨어 우선, `openh264` 소프트웨어 대체.** 더 낮은
  CPU/지연과 4K 여유를 위한 VA-API; `openh264`는 VA-API를 사용할 수 없는 곳에서 동작을
  유지합니다. 디코드된 프레임은 GPU에 머뭅니다 — VA-API 출력은 **DMA-BUF(무복사)**를 통해
  `wgpu`로 임포트되어 CPU 왕복 없이 표시되며; 소프트웨어 경로는 `wgpu` 텍스처에
  업로드합니다.
- **키맵: 하이브리드 — 호스트 XKB 기반에 내장 테이블 대체.** `xkbcommon`이 사용자의
  실제 호스트 레이아웃을 읽고(올바른 한글/CJK/비-US), 해결 가능한 호스트 키맵이 없을 때
  동봉된 테이블로 대체합니다.
- **라이브러리 경계: v0를 위한 `oxrdp-cli` 서브프로세스 + IPC.** 라운드 1의 얇은
  바이너리 선택과 일치합니다 — winpodx(Python)가 `oxrdp-cli`를 생성하고 소켓/JSON 제어
  채널을 통해 구동합니다. 인프로세스 FFI를 위한 C-ABI `cdylib`는 v0 이후의 선택지이며,
  v0 요구사항이 아닙니다.
