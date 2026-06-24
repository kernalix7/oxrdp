# 기능(Features)

[English](FEATURES.md) | **한국어**

> **상태: 프리알파(pre-alpha).** 아래의 어떤 것도 아직 배포 가능하지 않습니다. 이 페이지는
> 의도된 기능 집합과 그 마일스톤을 추적합니다. "v0" = winpodx의 FreeRDP 경로와의 드롭인
> 동등성에 필요함; "Staged" = 단계적 프로토콜 표면 결정에 따라 보류됨. 엔지니어링 세부 사항은
> [ARCHITECTURE.md](ARCHITECTURE.md)를, 순서는
> [로드맵](ARCHITECTURE.md#5-milestone-roadmap)을 참조하십시오.

## 핵심

oxrdp는 메모리 안전하고 처음부터 작성한 Rust RDP 스택 위에서 RAIL / RemoteApp을 통해
Windows 앱을 **네이티브 Linux 윈도**로 렌더링합니다 — 실제 제목, 실제 `WM_CLASS`,
고정 가능하고 alt-tab 전환 가능. RAIL은 부차적 사항이 아니라 핵심 기능입니다.

## 기능 매트릭스

| 기능 | 세부 사항 | 상태 |
|---|---|---|
| 연결 + 로그온 | TCP, X.224/MCS, 기능 교환 | v0 |
| TLS 보안 | `rustls`, 신뢰-최초-사용 / 핀 고정 / 시스템 CA 인증서 정책 | v0 |
| NLA / CredSSP | `sspi-rs`를 통한 NTLM / Kerberos | Staged(winpodx는 TLS를 통해 회피) |
| RAIL / RemoteApp | 원격 윈도 목록, z-순서, 팝업, 아이콘, 이동/크기 조정 | v0 |
| 네이티브 윈도 매핑 | 원격 윈도당 하나의 네이티브 toplevel; `WM_CLASS`, 제목, 아이콘 | v0 |
| 키보드 그랩 | `+grab-keyboard` 동등 | v0 |
| GFX 파이프라인 | H.264 AVC420 / AVC444 | v0 |
| RemoteFX | 협상된 대체 수단 | v0 |
| 비트맵 코덱 | 인터리브드 / 플래너, 최후의 수단 | v0 |
| 하드웨어 디코드 | VA-API, `wgpu`로의 DMA-BUF 무복사 | v0(소프트웨어 `openh264` 대체) |
| 클립보드 | cliprdr, 양방향 | v0 |
| 오디오 출력 | rdpsnd | v0 |
| 드라이브 리디렉션 | `\\tsclient`, rdpdr | v0 |
| 멀티 모니터 | RAIL-primary + span | v0 |
| HiDPI 스케일링 | 모니터별 스케일 팩터 | v0 |
| 동적 해상도 | 전체 데스크톱 크기 조정 | v0 |
| 마이크 입력 | audin | Staged |
| 프린터 리디렉션 | rdpdr 프린터 | Staged |
| USB / 스마트카드 / 시리얼 / 패러렐 | 장치 리디렉션 | Staged |
| 일반 RDP 서버 호환성 | 임의의(winpodx 외) 서버 | Staged |

## 디스플레이 백엔드

| 백엔드 | 라이브러리 | 상태 |
|---|---|---|
| X11 | `x11rb` | v0(우선) |
| Wayland | `smithay-client-toolkit` / `wayland-client` | v0(M4에서 동등성) |

둘 다 하나의 `DisplayBackend` 트레이트 뒤에 위치합니다; 프로토콜 코어는 둘 모두에 동일합니다.
