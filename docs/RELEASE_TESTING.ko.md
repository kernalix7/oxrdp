# 릴리스 테스팅(Release Testing)

[English](RELEASE_TESTING.md) | **한국어**

> **상태: 계획됨.** oxrdp는 프리알파입니다; 아직 테스트할 릴리스가 없습니다. 이 페이지는
> 클라이언트가 동작하면 릴리스별 스모크 + 검증 체크리스트를 담을 것입니다. 아래 개요는
> 기능이 안착함에 따라 체크리스트를 채울 수 있도록 의도를 기록합니다.

## CI가 커버할 수 있는 것과 없는 것

- **CI가 커버하는 것**: 순수한 sans-io 코어(`oxrdp-pdu`, `oxrdp-core`, `oxrdp-graphics`,
  `oxrdp-channels`, `oxrdp-rail`): 단위 테스트와 캡처된 트레이스 재생, 그리고 `fmt`,
  `clippy`, 의존성 `audit`. RDP 서버가 필요 없습니다.
- **CI가 커버할 수 없는 것**: 네트워크, 실제 윈도잉 시스템, GPU, 실제 Windows RDP 서버를
  다루는 셸 크레이트(`oxrdp-io`, `oxrdp-display`, `oxrdp-render`,
  `oxrdp-input`). 이들은 릴리스 전에 실제 게스트를 대상으로 수동으로 스모크 테스트해야 합니다.

## 계획된 수동 스모크 체크리스트(채워질 예정)

실제 Windows RDP 서버(예: winpodx dockur/windows 게스트)를 대상으로:

- [ ] 연결 + TLS 핸드셰이크 + 로그온
- [ ] 전체 데스크톱 세션이 렌더링되고 키보드/마우스 입력을 받음(X11)
- [ ] 단일 RAIL 윈도가 올바른 `WM_CLASS`, 제목, 아이콘으로 매핑됨
- [ ] 여러 RAIL 윈도: z-순서, 팝업, 포커스
- [ ] 클립보드 양방향
- [ ] 오디오 출력
- [ ] `\\tsclient` 드라이브 리디렉션 읽기/쓰기
- [ ] 멀티 모니터(primary + span) 및 HiDPI 스케일링
- [ ] Wayland 백엔드가 X11과 동등성
- [ ] VA-API 하드웨어 디코드 경로, 그리고 `openh264` 소프트웨어 대체
