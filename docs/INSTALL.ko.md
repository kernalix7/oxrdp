# 설치(Installation)

[English](INSTALL.md) | **한국어**

> **상태: 계획됨 — 아직 설치 불가.** oxrdp는 프리알파입니다; 아직 빌드 가능한 클라이언트나
> 릴리스 아티팩트가 없습니다. 이 문서는 첫 사용 가능한 마일스톤(see the
> [로드맵](ARCHITECTURE.md#5-milestone-roadmap))이 설치할 수 있는 무언가를 산출할 때
> 채워질 자리표시자입니다. 동작하지 않는 단계를 설명하기보다 의도적으로 지침을 비워 두었습니다.

## 현재로서는(개발자 전용)

워크스페이스가 구성되는 대로 빌드하려면 [CONTRIBUTING.md](../CONTRIBUTING.md)를
참조하십시오. Cargo 워크스페이스가 존재하면:

```bash
git clone https://github.com/kernalix7/oxrdp.git
cd oxrdp
cargo build --workspace
```

## 계획된 배포

배포 채널(crates.io, 배포판 패키지, 사전 빌드된 바이너리)은 v0에 가까워질 때 결정되어
여기에 문서화될 것입니다. oxrdp는 [winpodx](https://github.com/kernalix7/winpodx)가
라이브러리 + 얇은 바이너리로 소비하며; winpodx 설치 흐름이 배포되면 이를 가져올 것입니다.
