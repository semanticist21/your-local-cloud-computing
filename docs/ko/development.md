[English](../development.md) | 한국어

# 진단과 개발

`ylcc doctor`는 로컬 기능과 설정된 워커를 보고합니다. 서비스 로그, 서비스 계정의 실제 `PATH`, 토큰 권한, 비공개 HTTPS 도달 가능성, 컨테이너 런타임 접근을 먼저 확인하세요. 워커 연결 끊김은 명령 실패와 다릅니다. 허브 데이터베이스/블롭 디렉터리와 워커 데이터는 영구 보존하세요. 상태를 알 수 없는 시도의 프로세스가 아직 실행 중일 수 있다면 그 파일을 지우지 마세요.

```bash
(
  cd "$YLCC_SUPPORT_DIR"
  cargo test --locked
  bash scripts/test-install.sh
  cargo build --locked --release
)
```

CI는 네이티브 Linux x86_64, Linux arm64, Intel macOS, Apple Silicon macOS, Windows x86_64에서 빌드/테스트합니다. Windows 설치 스크립트 검사는 Windows PowerShell 5.1에서 실행됩니다. Windows 컨테이너 CI는 Server 2022 Docker 엔진을 쓰며, 일회용 named volume을 초기화하고, 실제 ContainerUser 잡을 실행/결과 수집하고, 하위 프로세스를 취소하고, 재생성 후 데이터가 유지되는지 확인합니다. 릴리스 자동화는 Unix 아카이브 4개와 Windows ZIP, 체크섬, Linux 멀티 플랫폼 이미지, 별도의 Windows LTSC 2022 이미지를 만들고 GitHub Release를 게시합니다.

릴리스하려면 깨끗한 main 체크아웃에서 `scripts/release.py X.Y.Z`를 실행하고 푸시하세요. 이 스크립트가 `Cargo.toml`, `Cargo.lock`과 고정된 설치 참조를 모두 한 커밋으로 올리고, Release 워크플로가 그 커밋에 `vX.Y.Z` 태그를 달아 모든 빌드가 통과하면 게시합니다. 태그를 직접 푸시하지 마세요. CI가 `scripts/release.py --check`를 실행하므로, `Cargo.toml`과 다른 고정 버전이 있으면 빌드가 실패합니다. 릴리스 빌드만 버전 번호 그대로 표시하고, 그 밖의 빌드는 `X.Y.Z+g<커밋>`(커밋되지 않은 변경이 있으면 `.dirty` 추가), Git 없이 빌드하면 `X.Y.Z+source`로 표시합니다.
