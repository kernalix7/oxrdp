# 비교(Comparison)

[English](COMPARISON.md) | **한국어**

> **상태: 프리알파(pre-alpha).** 이 문서는 oxrdp의 *의도된* 설계를 기존 선택지와 비교합니다.
> oxrdp는 아직 동작하지 않습니다; 이를 벤치마크가 아니라 목표의 진술로 받아들이십시오.

## 왜 그냥 FreeRDP를 계속 쓰지 않는가?

winpodx는 현재 RemoteApp / RAIL을 위해 `xfreerdp3`(FreeRDP 3.x)를 구동합니다. 동작하지만,
그 의존성은 oxrdp를 촉발한 반복되는 고통의 원천입니다:

- RAIL 윈도 매핑 정확성이 FreeRDP 포인트 릴리스마다 달라집니다(z-순서, 누락된 윈도,
  팝업, 작업 표시줄 / `WM_CLASS` 매핑).
- 대규모 C 코드베이스가 임계 경로에 있습니다; 충돌과 정의되지 않은 동작이 사용자에게 도달합니다.
- GFX / 멀티 모니터 / HiDPI / 클립보드 / 오디오가 각각 버전별 특이점을 안고 있습니다.

oxrdp의 명제는 첫날부터 RAIL과 네이티브 Linux 윈도 통합을 중심으로 설계하여
**안전한 Rust로 프로토콜 스택을 소유하는** 것입니다.

## oxrdp 대 대안들

| | **oxrdp** | **FreeRDP 3.x** | **IronRDP** | **winapps** |
|---|---|---|---|---|
| 언어 / 안전성 | Rust, 메모리 안전 코어 | C | Rust, 메모리 안전 | 셸 + FreeRDP |
| 접근 방식 | 처음부터 작성한 스택 | 성숙한 C 스택 | Rust RDP 라이브러리 | FreeRDP RemoteApp 래퍼 |
| RAIL / RemoteApp 초점 | 주요 설계 목표 | 지원되나 특이함 | 부분적 | FreeRDP 경유 |
| Linux 네이티브 윈도 통합 | 내장(X11 + Wayland) | X11의 RAIL | 초점 아님 | FreeRDP 경유 |
| 렌더링 | `wgpu` GPU + VA-API 디코드 | 소프트웨어 / GDI 경로 | 앱이 제공 | FreeRDP 경유 |
| 아키텍처 | sans-io, 퍼징 가능한 코어 | 모놀리식 | sans-io | 해당 없음 |
| 관계 | winpodx의 엔진 | oxrdp가 대체하는 것 | 설계 영감 | 독립적 선행 사례 |

## IronRDP에 대하여

[IronRDP](https://github.com/Devolutions/IronRDP)는 유능하고 메모리 안전한 Rust RDP
스택이며, 그 sans-io 설계는 oxrdp의 아키텍처에 직접 영향을 주었습니다. 그럼에도 oxrdp는
자체 스택을 처음부터 구현합니다 — 이는 oxrdp가 존재하는 바로 그 이유인 RAIL 의미론과
Linux 네이티브 윈도 통합에 대한 완전한 제어를 외부 스택 위에 끼워 맞추기보다 유지하기 위한,
의도적이고 눈을 똑바로 뜬 결정입니다. IronRDP 소스는 복사되지 않습니다.

## winapps에 대하여

[winapps](https://github.com/winapps-org/winapps)는 FreeRDP RemoteApp을 통해 Windows
앱을 표시하는 독립적 선행 사례입니다. 이는 기반이 아니라 참조점입니다; oxrdp는 그 아래의
FreeRDP 엔진을 래핑하는 대신 대체합니다.
