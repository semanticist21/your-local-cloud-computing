[English](../containers.md) | 한국어

# Docker와 Podman

워커 이미지는 Linux amd64와 arm64를 지원합니다. YLCC, CA 인증서, Git, Bash, curl이 들어 있으며, 워크로드 도구는 재사용 환경이나 직접 만든 이미지에 넣으세요. 기본 이미지에는 런타임 소켓이나 중첩 컨테이너 데몬이 없습니다.

기존 Docker나 Podman 런타임을 쓰세요. macOS에서는 기존 [OrbStack](https://docs.orbstack.dev/docker/)이나 Docker Desktop 설치로 Linux 워커를 실행할 수 있으며, YLCC는 두 번째 VM을 요구하지 않습니다. 전역 컨텍스트를 바꾸지 말고 명령마다 원하는 Docker 컨텍스트를 명시적으로 고르세요. 런타임의 이미지 캐시는 설정된 디스크에 두고, 원한다면 소유권이 강제되는 외부 볼륨에서 워커 데이터를 바인드 마운트하세요. 선택한 런타임이 의도한 사용자로 그 마운트와 보호된 토큰에 접근할 수 있는지 확인하세요.

[지원 체크아웃](install.md#예제와-에이전트-지원-파일-내려받기)을 사용해, Compose 허브는 호스트 루프백에만 공개하고 현재 사용자가 소유한 영구 디렉터리를 바인드하세요. 워커는 허브의 네트워크 네임스페이스를 공유하고 루프백으로 연결하므로 평문 HTTP가 로컬에만 머뭅니다. 토큰의 소유자 전용 모드를 유지하세요.

```bash
docker context ls
export YLCC_DOCKER_CONTEXT="$(docker context show)"
# To select an existing OrbStack context explicitly: export YLCC_DOCKER_CONTEXT=orbstack
docker --context "$YLCC_DOCKER_CONTEXT" info
export YLCC_UID="$(id -u)" YLCC_GID="$(id -g)"
export YLCC_DATA_DIR="$HOME/.local/share/ylcc/containers"
# For external work data, choose an already mounted ownership-enforced volume:
# export YLCC_DATA_DIR=/Volumes/YourOwnedVolume/ylcc/containers
export YLCC_TOKEN_FILE="$HOME/.config/ylcc/token"
mkdir -p "$YLCC_DATA_DIR/hub" "$YLCC_DATA_DIR/worker"
docker --context "$YLCC_DOCKER_CONTEXT" compose -f "$YLCC_SUPPORT_DIR/compose.yaml" up -d
```

워커는 공유 네트워크 네임스페이스 안에서 `http://127.0.0.1:7443`으로 연결합니다. 대신 기존 비공개 허브에 합류하려면, 영구 데이터 마운트를 가진 워커만 실행하세요.

```bash
mkdir -p "$YLCC_DATA_DIR/worker"
docker --context "$YLCC_DOCKER_CONTEXT" run -d --init --name ylcc-worker --restart unless-stopped \
  --user "$YLCC_UID:$YLCC_GID" \
  --mount "type=bind,src=$YLCC_DATA_DIR/worker,dst=/data" \
  --mount "type=bind,src=$YLCC_TOKEN_FILE,dst=/run/secrets/token,readonly" \
  ghcr.io/semanticist21/your-local-cloud-computing:0.5.14 \
  --hub https://HUB.example.ts.net:10000 --token-file /run/secrets/token \
  worker --id container-worker --data-dir /data --max-jobs 1
```

rootless Podman 서비스로 관리하려면 `$YLCC_SUPPORT_DIR/examples/quadlet/ylcc-worker.container`([예제](../../examples/quadlet/ylcc-worker.container))를 `~/.config/containers/systemd/`에 복사하고, 허브 URL과 워커 ID를 바꾼 뒤 `systemctl --user daemon-reload && systemctl --user start ylcc-worker`를 실행하세요. keep-id 매핑 덕분에 서비스가 모드 600 토큰을 읽을 수 있습니다. [Podman Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html)을 참고하세요.

## 선택 사항: 형제 컨테이너 실행

네이티브 워커는 이미 구성된 Docker나 Podman 런타임을 쓸 수 있습니다. 워커 서비스 계정의 `HOME`에서 컨텍스트와 소켓 접근을 확인하세요. 워크로드 이미지를 요청하려면 `--container IMAGE`를 넘기거나 이미지 환경을 쓰세요. 컨테이너화된 워커에는 명시적으로 설치한 런타임 클라이언트와 호스트 런타임 접근이 필요합니다. 버전이 맞는 릴리스 바이너리로 선택적 클라이언트 타깃을 빌드하세요.

```bash
mkdir -p "$YLCC_SUPPORT_DIR/dist"
# Extract the verified Linux release archive for the runtime CPU; arm64 example:
tar -xzf /path/to/ylcc-linux-aarch64.tar.gz -C "$YLCC_SUPPORT_DIR/dist"
docker --context "$YLCC_DOCKER_CONTEXT" build --target runtime-client -t ylcc-runtime-client:0.5.14 \
  -f "$YLCC_SUPPORT_DIR/Dockerfile" "$YLCC_SUPPORT_DIR"
```

데몬 소켓을 명시적으로 마운트하고 소켓의 그룹을 워커에 추가하세요. **이렇게 하면 워커가 그 호스트 런타임을 제어할 수 있게 됩니다.** 워커 데이터 디렉터리를 호스트와 워커에서 **같은 절대 경로**로 바인드 마운트하고, 그 경로를 `--data-dir`로 넘기세요. 형제 컨테이너의 마운트는 워커 컨테이너가 아니라 호스트 데몬이 해석하기 때문입니다. 이 구성은 클라이언트만 쓰며 Docker-in-Docker는 절대 쓰지 않습니다. Docker Desktop의 경로 공유와 rootless Podman 소켓 권한도 선택한 경로/사용자를 허용해야 합니다.

Docker 소켓의 경우 `YLCC_RUNTIME_SOCKET`을 소켓의 절대 경로로, `YLCC_RUNTIME_GID`를 숫자 그룹 ID로 설정한 뒤:

```bash
: "${YLCC_RUNTIME_SOCKET:?Set the Docker socket path}" "${YLCC_RUNTIME_GID:?Set its numeric group ID}"
export YLCC_RUNTIME_DATA="$HOME/.local/share/ylcc/runtime-worker"
mkdir -p "$YLCC_RUNTIME_DATA"
docker --context "$YLCC_DOCKER_CONTEXT" run -d --init --name ylcc-runtime-worker --restart unless-stopped \
  --user "$(id -u):$(id -g)" --group-add "$YLCC_RUNTIME_GID" \
  --mount "type=bind,src=$YLCC_RUNTIME_SOCKET,dst=/var/run/docker.sock" \
  --mount "type=bind,src=$YLCC_RUNTIME_DATA,dst=$YLCC_RUNTIME_DATA" \
  --mount "type=bind,src=$YLCC_TOKEN_FILE,dst=/run/secrets/token,readonly" \
  ylcc-runtime-client:0.5.14 \
  --hub https://HUB.example.ts.net:10000 --token-file /run/secrets/token \
  worker --id runtime-worker --data-dir "$YLCC_RUNTIME_DATA" --max-jobs 1
```

커스텀 워크로드 이미지는 YLCC 밖에서 빌드해 GHCR/Forgejo 등의 레지스트리에 푸시하고, 변하지 않는 다이제스트로 등록하세요. 런타임 자격 증명은 서비스 사용자에게 두고, 환경 카탈로그에는 넣지 마세요.
