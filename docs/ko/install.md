[English](../install.md) | 한국어

# 설치

[README](../../README.ko.md#cli-설치)에 나온 방법으로 바이너리를 설치하세요.

설치 위치를 바꾸려면 `--prefix /your/prefix`를 쓰세요. [릴리스 아카이브](https://github.com/semanticist21/your-local-cloud-computing/releases)를 직접 내려받아 확인해도 됩니다. macOS 빌드는 서명되지 않은 명령줄 프로그램이며, 공증(notarize)된 앱 번들이 아닙니다. Linux 릴리스 바이너리는 네이티브 워커가 GPU 드라이버 라이브러리를 불러올 수 있도록 glibc 2.35 이상이 필요합니다. Alpine 같은 musl 기반 시스템에서는 컨테이너 이미지를 쓰거나 해당 시스템용으로 소스에서 빌드하세요. GPU 감지에는 추가로 동적 라이브러리 로딩이 필요합니다. 워크로드 바이너리는 각자의 플랫폼/ABI 요구 조건을 그대로 따릅니다.

## 예제와 에이전트 지원 파일 내려받기

바이너리 설치 스크립트는 실행 파일과 라이선스 고지만 설치합니다. 이 문서들에 나오는 예제 잡, 환경 레시피, Compose/Quadlet 파일, 에이전트 설치 스크립트를 쓰기 전에, 버전이 맞는 지원 체크아웃을 애플리케이션 프로젝트 바깥에 받아 두세요.

```bash
export YLCC_SUPPORT_DIR="$HOME/.local/share/ylcc/support/v0.4.6"
mkdir -p "$(dirname "$YLCC_SUPPORT_DIR")"
git clone --depth 1 --branch v0.4.6 \
  https://github.com/semanticist21/your-local-cloud-computing.git "$YLCC_SUPPORT_DIR"
```

이 문서들의 예제를 위해 이 디렉터리를 유지하세요. 이미 클론했다면 재사용하면 됩니다. 이 디렉터리에는 공개 지원 파일만 있으며 풀 토큰은 절대 들어가지 않습니다. 명령은 이 디렉터리를 명시적으로 참조하거나 서브셸에서 예제를 실행하므로, 애플리케이션의 작업 디렉터리는 그대로 유지됩니다.

바이너리를 내려받는 대신 이 체크아웃에서 CLI를 빌드하려면:

```bash
npm ci --prefix "$YLCC_SUPPORT_DIR/dashboard"
npm run build --prefix "$YLCC_SUPPORT_DIR/dashboard"
cargo build --manifest-path "$YLCC_SUPPORT_DIR/Cargo.toml" --locked --release
mkdir -p "$HOME/.local/bin"
install -m 755 "$YLCC_SUPPORT_DIR/target/release/ylcc" "$HOME/.local/bin/ylcc"
```

## 기존 설치 업데이트

원하는 버전으로 설치 스크립트를 다시 실행하세요. 설정/토큰/데이터는 보존됩니다. 백그라운드 워커는 [`ylcc workers update`](services.md#워커-업데이트)로 업데이트하세요.
