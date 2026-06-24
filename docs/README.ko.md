# oxrdp

[![license](https://img.shields.io/github/license/kernalix7/oxrdp?style=flat-square&color=blue)](LICENSE)
[![status](https://img.shields.io/badge/status-pre--alpha-orange?style=flat-square)](#status)
[![language](https://img.shields.io/badge/rust-stable-DEA584?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)

[English](../README.md) | **한국어**

**Windows 애플리케이션을 Linux 데스크톱에 매끄럽게 통합하기 위해 Rust로 작성한 메모리 안전 RDP 클라이언트.**

oxrdp는 Windows RDP 서버에 연결하여 RAIL / RemoteApp을 통해 원격 앱을 네이티브 Linux 윈도로 렌더링합니다 — 실제 제목, 실제 `WM_CLASS`, 고정 가능하고 alt-tab 전환 가능 — Rust의 안전성 보장, 최소 자원 사용, 높은 성능을 우선합니다.

oxrdp는 [winpodx](../00G_winpodx) 뒤에서 동작하는 RDP 엔진으로, 독립형 프로젝트로 분리되었습니다. 이는 winpodx의 현재 FreeRDP 3.x 의존성을 대체하고, 그 의존성이 부과하는 한계를 해결하기 위해 존재합니다.

---

## oxrdp가 존재하는 이유

winpodx는 오늘날 `xfreerdp3`(FreeRDP 3.x)를 외부 호출하여 RemoteApp / RAIL을 통해 Windows 앱을 네이티브 Linux 윈도로 표시합니다. 이는 동작하지만, FreeRDP 의존성은 반복되는 고통의 원천입니다:

- **RAIL 윈도 매핑 정확성** — z-순서, 누락된 윈도, 팝업 / 드롭다운 / 툴팁, 작업 표시줄 및 `WM_CLASS` 매핑 버그가 FreeRDP 포인트 릴리스마다 달라집니다(예: 3.6.0 미만에서 깨진 RemoteApp 윈도).
- **성능 및 입력 지연** — GFX H.264 / AVC444 협상, 대역폭, 프레임 레이트, 왕복 입력 지연.
- **메모리 안전성 및 안정성** — 임계 경로에 있는 대규모 C 코드베이스; 충돌과 정의되지 않은 동작이 사용자에게 떨어집니다.
- **기능 격차 및 마찰** — 클립보드, 오디오 입/출력, 멀티 모니터 전략, HiDPI 스케일링, 장치 리디렉션이 각각 FreeRDP 버전별 특이점을 안고 있습니다.

oxrdp의 명제: **안전한 Rust로 프로토콜 스택을 소유하라**. 첫날부터 RAIL과 네이티브 Linux 윈도 통합을 중심으로 설계하여, 이것들을 우회해야 할 업스트림 특이점이 아니라 우리가 제어하는 엔지니어링 결정으로 만듭니다.

## 프로젝트 결정 사항(확정)

| 결정 | 선택 | 근거 |
| --- | --- | --- |
| **프로토콜 스택** | Rust로 처음부터 구현 | 완전한 제어, FreeRDP 의존성 제로, 진정한 메모리 안전 코어. |
| **저수준 빌딩 블록** | 검증된 크레이트 재사용 | TLS는 `rustls`+`ring`; NLA/CredSSP는 `sspi-rs`(보류); 비디오 디코드는 `openh264`/`dav1d` 바인딩; 비동기 IO는 `tokio`. "처음부터" = RDP 프로토콜, RAIL, 렌더링 — 암호/코덱 프리미티브가 아님. |
| **코어 아키텍처** | sans-io 상태 머신 | 순수하고 IO가 없는 프로토콜 코어(IronRDP 방식)와 플러그형 IO / 렌더 / 입력 셸. 테스트 가능성, 퍼징, X11+Wayland 재사용을 얻습니다. |
| **디스플레이 백엔드** | 하나의 추상화 뒤의 X11 + Wayland | `DisplayBackend` 트레이트; 각 원격 RAIL 윈도가 하나의 네이티브 toplevel에 매핑됩니다. X11 백엔드 우선(오늘날의 배포와 일치), Wayland 병행. |
| **렌더링 및 디코드** | 시작부터 `wgpu` GPU; VA-API HW 디코드 | 합성/스케일링/표시(present)는 `wgpu`를 통해. H.264 GFX 디코드는 VA-API 하드웨어 우선이며 `openh264` 소프트웨어 대체 수단을 둡니다; VA-API 프레임은 DMA-BUF를 통해 `wgpu`로 임포트됩니다(무복사). |
| **키맵** | 하이브리드(호스트 XKB + 테이블 대체) | `xkbcommon`이 호스트 레이아웃을 읽습니다(올바른 한글/CJK), 해결되는 것이 없으면 동봉된 테이블로 대체합니다. |
| **프로토콜 표면** | 단계적 | v0는 현대적이고 좁은 표면을 강제합니다(게스트를 우리가 제어); 이후 일반 RDP 서버 호환성으로 넓혀갑니다. |
| **winpodx 통합** | Rust 라이브러리 + 얇은 바이너리; v0 = `oxrdp-cli` 서브프로세스 + IPC | winpodx(Python)가 `oxrdp-cli`를 생성하고 소켓/JSON 제어 채널을 통해 구동합니다. 인프로세스 C-ABI `cdylib` FFI는 v0 이후의 선택지입니다. |
| **v0 성공 기준** | **winpodx의 FreeRDP 경로와의 드롭인 동등성** | winpodx가 `xfreerdp3` 대신 oxrdp에서 RAIL 멀티앱 워크플로를 동등하게 실행할 때 v0가 "완료"됩니다. |

## 범위

**범위 내(궁극적으로):** winpodx가 의존하는 FreeRDP 기능의 전체 집합 — [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)의 동등성 매트릭스 참조. RAIL/RemoteApp은 부차적 사항이 아니라 핵심 기능입니다.

**v0 목표 표면(드롭인 동등성):**
- 신뢰-최초-사용(trust-on-first-use) 인증서(`/cert:tofu|ignore`)와 함께 **TLS 보안**(`/sec:tls`)을 통한 연결 + 로그온(`/v /u /d /p`). **NLA/CredSSP는 보류** — winpodx는 NLA 경로를 피하기 위해 의도적으로 `/sec:tls`를 사용하므로, v0는 이것이 필요하지 않습니다.
- RAIL / RemoteApp 실행(`/app:program,name,cmd`), `WM_CLASS` 매핑, 키보드 그랩.
- 그래픽: RemoteFX 대체 수단을 갖춘 GFX 파이프라인(H.264 AVC420/AVC444); 최후의 수단으로 비트맵.
- 채널: 클립보드(cliprdr), 오디오 출력(rdpsnd), 파일시스템 리디렉션(`\\tsclient`, rdpdr).
- 디스플레이: 멀티 모니터(RAIL-primary + span), HiDPI 스케일링, 동적 해상도(데스크톱 모드).

**보류 / 단계적:** NLA/CredSSP 및 Kerberos, 마이크(audin), 프린터, USB / 스마트카드 / 시리얼 / 패러렐 리디렉션, 임의의(winpodx가 제어하지 않는) RDP 서버와의 광범위한 호환성.

## 상태

프리알파 — 명세 및 스캐폴딩. 아직 동작하는 클라이언트는 없습니다. 워크스페이스 구성과 마일스톤 로드맵은 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) 참조.

## 라이선스

미정(허용적 라이선스 의도 — Apache-2.0 / MIT, Rust 생태계 및 winpodx의 배포 모델과 일치).
