[English](../environments.md) | 한국어

# 재사용 환경

[지원 체크아웃](install.md#예제와-에이전트-지원-파일-내려받기)에서 Rust, Node, Python 이미지 환경을 등록하고 준비합니다.

```bash
ylcc env add "$YLCC_SUPPORT_DIR/examples/environments/rust.json"
ylcc env prepare rust-linux --worker linux-build
ylcc env status
(
  cd "$YLCC_SUPPORT_DIR"
  ylcc run --input examples/jobs/hello.rs --os linux --arch x86_64 \
    --environment rust-linux --output result.json \
    -- sh -c 'rustc examples/jobs/hello.rs -o hello && ./hello'
)
```

예제의 이미지 참조는 멀티 플랫폼 다이제스트로 고정되어 있습니다. [Node](../../examples/environments/node.json)나 [Python](../../examples/environments/python.json)도 같은 방식으로 쓰세요. 이미지 환경은 쓸 수 있는 컨테이너 런타임이 있는 워커에서 실행됩니다. macOS의 Docker도 Linux를 실행합니다. 이미지는 실제 실행 환경에서 내려받아 검증하며, 해석된 다이제스트와 준비 상태는 워커별로 저장됩니다.

네이티브 Linux/macOS 도구의 경우, [rustup 레시피](../../examples/environments/rust-native.json)가 고정된 툴체인을 환경의 사용자 소유 디렉터리에 설치합니다. 프로젝트별 레시피는 mise 같은 기존 버전 관리자, 릴리스 바이너리, Git 클론/빌드 명령을 쓸 수 있습니다. 명령은 `YLCC_ENV_DIR`을 받으며, 그 `bin` 디렉터리가 `PATH` 앞에 추가됩니다. 환경 값에서는 `${YLCC_ENV_DIR}`과 `${PATH}`가 확장됩니다. 설치 단계는 반복 가능하고 버전이 고정되게 유지하고, 기대한 도구를 쓸 수 없으면 실패하는 `verify` 명령을 제공하세요.

환경을 바꿀 때는 새 `revision`을 쓰고, `env add`와 `env prepare`를 명시적으로 실행하세요. `ylcc env delete ID`는 카탈로그에서 환경을 상태 및 준비 로그와 함께 제거합니다. id가 이미 없으면 성공하고, 대기 중이거나 실행 중인 잡이 그 환경을 쓰고 있으면 거부합니다. 워커에 이미 준비된 파일은 데이터 디렉터리에 남습니다. 지난 잡은 기록된 환경을 유지하며, 그런 잡을 다시 시도하려면 id를 다시 등록해야 합니다. 준비/업데이트는 직렬화되며 워커의 진행 중인 작업을 비운 뒤 진행됩니다. 잡은 처음 사용할 때 요청한 네이티브 환경을 준비할 수 있습니다. 이미지 환경은 변경할 수 없으므로, 설정은 일회성 setup 명령이 아니라 이미지 Dockerfile에 넣으세요. 영구 환경/캐시 데이터는 워커 데이터 디렉터리 아래에 있으며, 컨테이너 워커라면 마운트되어 있어야 합니다. 워커 이미지 업데이트는 잡을 비운 뒤 이 마운트를 유지한 채 명시적으로 pull/재생성하는 작업입니다.

API CLI를 설치한다고 인증까지 되지는 않습니다. 필요한 자격 증명은 서비스 사용자의 기존 보안 설정으로 제공하세요. 드라이버, 시스템 SDK, Xcode는 호스트 설정으로 남으며, 이 도구가 자동으로 바꾸지 않습니다.
