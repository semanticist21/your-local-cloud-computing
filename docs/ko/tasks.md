[English](../tasks.md) | 한국어

# 프로젝트 태스크

반복해서 쓰는 명령은 애플리케이션 저장소의 `ylcc.yaml`에 두세요. 요청자는 기존 사용자 수준의 허브/토큰 설정을 사용하므로, 이 파일에 자격 증명을 넣지 마세요. 명령과 선택 경로는 다른 위치에서 실행하더라도 YAML 파일이 있는 디렉터리를 기준으로 합니다.

```yaml
version: 1
tasks:
  test:
    command: [cargo, test, --locked]
    inputs: [Cargo.toml, Cargo.lock, src, tests]
    requirements:
      os: linux
      arch: x86_64
      tools:
        cargo: "*"
    timeout_secs: 1800
    priority: 0
```

프로젝트의 실제 입력과 명령을 쓰세요. 내려받을 파일은 `outputs`, 도구 환경은 `environment`로 추가하고, 프로젝트 전체를 선택하려면 `inputs` 대신 `project: true`를 쓰세요. 소스 입력이 없는 명령은 명시적으로 `no_input: true`를 설정해야 합니다. 프로젝트 파일은 실행 가능한 명령을 정의하므로, 새 파일은 실행 전에 내용을 확인하세요.

```bash
ylcc task list
ylcc task run test
ylcc task run test --file /path/to/application/ylcc.yaml --detach
ylcc task delete test
```

`task delete`는 태스크 블록과 바로 위의 주석 줄을 지우고 나머지 파일은 그대로 둡니다. 태스크가 이미 없으면 성공합니다. 다른 태스크가 쓰는 YAML 앵커를 가진 태스크처럼, 지우면 파일이 유효하지 않게 되는 경우는 거부합니다. 저장된 태스크를 바꾸려면 직접 고치거나, 지운 뒤 다시 저장하세요.

## 반복 가능한 워크플로

프로젝트 파일 하나에 CI 워크플로처럼 실행에 필요한 모든 것을 담을 수 있습니다. 공통 기본값, 툴체인, OS별 타깃, 워커 쪽 캐시까지요.

```yaml
version: 1
environments:
  bun: {uses: bun@1.3.14}          # Built-in preset; `environment: bun@1.3.14` also works inline.
  tools:                           # Or a full recipe (setup/verify/env, or an image digest).
    setup: ['curl -fsSL https://example.com/tool.tar.gz | tar -xz -C "$YLCC_ENV_DIR/bin"']
    verify: [tool --version]
defaults:
  requirements: {os: linux, arch: x86_64}
  environment: bun
  timeout_secs: 1800
  cache: [node_modules]
tasks:
  test:
    command: [sh, -c, 'bun install --frozen-lockfile && bun test']
    inputs: [package.json, bun.lock, src]
  cargo-test:
    command: [cargo, test, --locked]
    inputs: [Cargo.toml, Cargo.lock, src]
    environment: rust-native       # A name not declared above refers to a Hub-registered environment.
    cache: [target]
    targets:                       # One job per target; each overrides the requirements.
      - {os: linux, arch: x86_64}
      - {os: macos, arch: aarch64}
```

- 파일에 선언한 **environments**는 `task run`이 사용할 때 등록됩니다. 각 워커는 처음 사용할 때 한 번 설치하고 유지하므로, 이후 실행은 바로 시작됩니다. 리비전은 레시피의 해시입니다. 레시피를 고치면 새 리비전을 준비하고, 바뀌지 않은 레시피는 재사용합니다. 커스텀 레시피의 허브 id에는 프로젝트 접미사가 붙으므로(예: `tools-3fa2…`), 같은 이름을 쓰는 프로젝트끼리 레시피를 덮어쓰거나 별도로 등록된 환경을 대체하지 않습니다.
- **프리셋**(`bun`, `node`, `rust`, `go`, `uv`)은 워커의 환경 디렉터리에 설치됩니다. Linux/macOS에서는 셸 레시피, Windows x86_64에서는 PowerShell 레시피(id 접미사 `-windows`)를 씁니다. 프리셋이 레시피를 고르려면 요구 조건이나 타깃에 `os`가 명시되어 있어야 합니다. Windows의 Rust는 호스트에 Visual Studio Build Tools가 설치된 경우에만 MSVC 타깃을 링크합니다.
- **defaults**는 해당 필드를 직접 설정하지 않은 모든 태스크에 적용됩니다. `env` 맵은 병합됩니다.
- **targets**는 항목마다 잡을 하나씩 제출합니다. 스냅샷은 한 번만 업로드됩니다. `task run`은 타깃이 하나면 객체 하나를, 여러 개면 배열을 출력하며, 잡 중 하나라도 실패하면 실패합니다.
- **cache** 디렉터리는 잡이 끝나면 체크아웃 밖으로 옮겨졌다가, 같은 워커에서 프로젝트, 환경, 컨테이너, 경로가 같은 다음 잡에 다시 들어갑니다. 프로젝트는 `name`을 설정했다면 그 값이고(요청자 간 캐시 공유용), 아니면 YAML 디렉터리의 절대 경로입니다. 캐시는 실패 후에도 유지됩니다. 다른 시도가 캐시를 쓰고 있으면 기다리지 않고 캐시 없이 실행합니다. Cargo의 `target`처럼 빌드 산출물이 절대 경로를 담기 때문에, 캐시를 쓰는 태스크는 프로젝트·환경·컨테이너별로 고정된 `workspaces/` 디렉터리에 체크아웃합니다(매번 새로 만듭니다). 같은 태스크의 동시 시도는 자기 디렉터리를 씁니다. 캐시는 워커 데이터 디렉터리의 `caches/` 아래에 있으며, [워커 디스크 정리](services.md#워커-디스크-정리)에 따라 지워집니다. 입력에 이미 있는 경로는 절대 교체하지 않습니다.

잘 동작한 임시 실행은 `--save-task NAME`으로 태스크로 만들 수 있습니다. 잡이 성공한 뒤(또는 `--detach`라면 제출 시점에) `./ylcc.yaml`에 추가합니다(없으면 만들고, 기존 주석은 유지). 입력과 명령은 현재 디렉터리 기준 상대 경로로 남습니다. 파일의 `defaults` 때문에 저장된 태스크가 다르게 실행될 상황이면 제출 전에 실행을 거부합니다.

```bash
ylcc run --input package.json --input bun.lock --input src --os linux --environment bun@1.3.14 \
  --cache node_modules --save-task test -- sh -c 'bun install && bun test'
ylcc task run test
```

YAML 버전 1은 알 수 없는 필드와 잘못된 선택을 거부합니다. 이 스키마는 CPU/메모리 비율 제한을 지원하지 않습니다. 실제 자원 상한은 컨테이너 런타임이나 운영체제에서 설정하세요. `max_jobs`는 동시 잡 수를 제한할 뿐, CPU나 메모리 사용량을 제한하지 않습니다.

## 대기 중인 작업 제어

허브는 대기열과 워커 제어 상태를 영구 저장합니다. 호환되는 대기 잡 중에서는 우선순위 숫자가 높은 잡이 먼저 실행되고, 우선순위가 같으면 제출 순서를 유지합니다. 우선순위 기본값은 `0`이며 `-100`부터 `100`까지 허용됩니다. 우선순위를 바꿔도 이미 할당되었거나 실행 중인 잡은 중단되지 않습니다.

```bash
ylcc run --no-input --os linux --priority 10 --detach -- true
ylcc jobs priority JOB_ID 20
ylcc workers pause WORKER_ID
ylcc workers resume WORKER_ID
ylcc workers limit WORKER_ID --max-jobs 1
```

일시 정지하면 새 할당과 새 환경 준비가 멈추고, 진행 중인 작업은 끝까지 실행됩니다. 재개하면 다시 스케줄링됩니다. 실시간 동시 실행 제한은 워커 시작 시 `--max-jobs` 용량까지만 올릴 수 있고, 낮추면 진행 중인 작업은 끝까지 실행됩니다. 시작 용량은 워커 서비스를 설치할 때 정하세요. 제어 상태는 허브 재시작과 워커 재연결 후에도 유지됩니다. 안전하게 업데이트하려면 워커를 일시 정지하고, 진행 중인 시도가 끝나길 기다린 뒤 업데이트하고 재개하세요. 취소는 여전히 잡별로 명시적으로 해야 합니다.

우선순위를 지정한 제출에는 우선순위 지원을 광고하는 허브가 필요합니다. 오래된 허브는 0이 아닌 우선순위를 조용히 무시하는 대신 거부됩니다.

## 설정과 빠른 태스크 템플릿

비밀 정보를 출력하지 않고 첫 실행 탐색을 합니다.

```bash
ylcc setup
ylcc examples
```

현재 애플리케이션 저장소에 프로젝트 태스크 파일을 만듭니다. 런타임 템플릿은 프리셋 환경과 캐시를 선언합니다.

```bash
ylcc task init --runtime bun --name test --file ylcc.yaml
ylcc task init --runtime rust --name test --file ylcc.yaml
ylcc task list
ylcc task run test --detach
```

준비된 환경은 믿고 쓰기 전에 재사용 상태를 확인하세요.

```bash
ylcc env status
ylcc env ensure my-tools --worker WORKER_ID
```
