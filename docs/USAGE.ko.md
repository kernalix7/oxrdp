# 사용법(Usage)

[English](USAGE.md) | **한국어**

> **상태: 계획됨 — 아직 사용 불가.** oxrdp는 프리알파입니다; 클라이언트가 아직 실행되지
> 않습니다. 이 페이지는 자리표시자입니다. `oxrdp-cli` 플래그 표면과 라이브러리 `Session`
> API가 존재하고 동작하면 이를 문서화할 것입니다. 그때까지는 존재하지 않는 기능을 암시하지
> 않기 위해 의도적으로 어떤 명령도 설명하지 않습니다.

## 의도된 형태

- **라이브러리**(`oxrdp` 크레이트): winpodx(및 기타)가 구동하는 상위 수준 `Session` API.
- **얇은 바이너리**(`oxrdp-cli`): RDP 서버를 대상으로 세션 — 전체 데스크톱, 또는 RAIL /
  RemoteApp 윈도 — 을 실행하며, 라이브러리를 디스플레이 백엔드에 연결합니다.
- **winpodx 통합**: v0의 경우, winpodx가 `oxrdp-cli`를 생성하고 소켓/JSON 채널을 통해
  제어합니다(see [ARCHITECTURE.md §6](ARCHITECTURE.md#6-winpodx-integration-shape)).

구체적인 플래그와 API는 M2–M5 마일스톤에서 여기에 문서화될 것입니다.
