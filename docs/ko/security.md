[English](../security.md) | 한국어

# 보안

YLCC는 신뢰할 수 있는 사람과 신뢰할 수 있는 코드를 위한 풀입니다. 역할 토큰, 감사 로그, 워커 명령 정책은 머신마다 할 수 있는 일을 제한하고 누가 무엇을 했는지 기록하지만, 풀을 멀티테넌트 CI로 바꾸지는 않습니다. [보호하지 않는 것](#보호하지-않는-것)을 참고하세요.

## 역할

허브를 시작할 때 쓴 토큰 파일(`ylcc init`)이 관리자용 **풀(pool)** 토큰입니다. 기존 단일 토큰 풀은 그대로 동작합니다. 다른 머신에는 세 역할 중 하나로 각자의 토큰을 발급하세요.

| 작업 | admin | requester | worker |
| --- | --- | --- | --- |
| 잡 제출, 목록, 취소, 우선순위 변경(모든 잡); 로그 읽기 | 예 | 예 | 아니요 |
| 블롭 업로드와 다운로드 | 예 | 예 | 예 |
| `env add`, `prepare`, `status`, `logs`; 워커 목록 | 예 | 예 | 아니요 |
| 대시보드 | 예, 감사 로그와 토큰 페이지 포함 | 예 | 아니요 |
| 워커로 연결 | 모든 워커 ID | 아니요 | 자기 `--worker-id`만 |
| 워커 삭제, 일시 정지, 재개, 제한, 업데이트; `env delete` | 예 | 아니요 | 아니요 |
| `ylcc tokens`, `ylcc audit` | 예 | 아니요 | 아니요 |

알 수 없거나 폐기된 토큰은 `401`을, 해당 작업이 허용되지 않은 역할은 `403`을 받습니다. 워커 토큰은 자기 워커를 일시 정지할 수 없습니다. 요청자는 다른 사람의 잡도 취소하거나 우선순위를 바꿀 수 있으며, 이는 감사 로그에 기록됩니다.

## 토큰

허브를 먼저 업그레이드하세요. 이전 허브는 범위가 지정된 토큰을 `401`로 거부합니다. `tokens`와 `audit`은 허브의 `/health` 기능 목록을 확인하고, 기능이 없으면 "upgrade the Hub first"라고 알려 줍니다.

```bash
ylcc tokens create --name laptop --role requester --out laptop.token
ylcc tokens create --name linux-build --role worker --worker-id linux-build --out linux-build.token
ylcc tokens list
ylcc tokens revoke TOKEN_ID
```

`create`는 비밀 값(`ylcc_`와 16진수 64자)을 소유자만 읽을 수 있는 새 파일에 쓰며, 출력하지 않고 기존 파일을 덮어쓰지 않습니다. 허브는 SHA-256 해시만 저장합니다. 워커 토큰에는 `--worker-id`가 필수입니다. `pool`이라는 이름은 예약되어 있습니다. 토큰을 폐기하면 그 토큰을 쓰는 워커의 연결이 끊기며, 진행 중인 요청은 끝까지 처리됩니다.

### 공유 풀 토큰에서 옮기기

이 과정을 마치기 전까지는 풀 토큰을 가진 모든 머신이 사실상 관리자입니다.

1. 허브를 업그레이드합니다.
2. 워커마다 `--worker-id`를 지정한 워커 토큰을 하나씩, 노트북이나 에이전트마다 요청자 토큰을 하나씩 만듭니다.
3. 각 파일을 안전한 관리 채널로 전달하고 모드 `600`으로 둡니다.
4. 각 워커와 요청자를 자기 `--token-file`로 다시 시작합니다(또는 서비스를 다시 설치하거나 `configure`를 다시 실행합니다).
5. 관리자만 갖도록 풀 토큰을 교체합니다. 허브를 멈추고, 토큰 파일을 지우고, `ylcc init`을 실행한 뒤 허브를 시작합니다.

머신의 토큰을 교체하려면 새 토큰을 만들어 머신을 전환한 뒤 이전 ID를 폐기하세요.

## 감사 로그

허브는 `token.create`/`revoke`, `job.submit`/`cancel`/`priority`, `worker.control`/`update`/`delete`, `environment.upsert`(새 리비전만)/`delete`/`prepare`, 그리고 `worker.connect`(토큰이나 버전이 바뀔 때만)와 `worker.rejected`를 기록합니다. 마지막 두 가지는 토큰당 시간당 20건으로 제한되며, 그 뒤에는 `worker.audit_suppressed` 한 건이 기록됩니다. 읽기 요청과 `401`/`403` 응답은 기록하지 않습니다. 최신 이벤트 10,000건을 보관합니다.

```bash
ylcc audit --limit 50
```

각 잡에는 허브가 설정한 토큰 이름인 `submitted_by`가 기록됩니다. 대시보드는 풀 토큰이나 요청자 토큰으로 로그인하며, 계정 이름과 역할을 보여 주고 잡에는 **Submitted by**가 표시됩니다. 관리자에게는 읽기 전용 **Audit log**와 **Tokens** 페이지도 보입니다. 토큰 관리는 CLI로 하세요. 워커 토큰으로는 안내 화면만 보입니다.

## 워커 명령 정책

워커는 실행할 작업을 선택적으로 제한할 수 있습니다.

```bash
ylcc worker --id linux-build --data-dir "$HOME/.local/share/ylcc/worker" --policy /etc/ylcc/policy.yaml
```

`service install --role worker`에도 같은 `--policy FILE`을 쓸 수 있습니다. 정책 파일이 없으면 워커는 이전처럼 배정된 모든 작업을 실행합니다.

### 형식

```yaml
version: 1
commands:            # argv prefixes; each element exact, "*" any, or "prefix*"
  - [cargo, build]
  - [make, "test-*"]
containers: ["ghcr.io/me/*"]  # exact image, "*" or "prefix*" (end prefixes at / : or @)
environments:                 # id exact/"*"/"prefix*"; sha256 = content digest or "*"
  - {id: rust-1.96.0, sha256: <64 hex>}
env: [RUST_LOG]               # allowed job env keys; PATH, LD_*, DYLD_* only via "*"
self_update: false            # default true
```

- 생략한 범주는 제한하지 않으며, `[]`는 그 범주를 모두 거부합니다. 비어 있는 범주(뒤에 아무것도 없는 `env:`), 철자가 틀린 키, 단독이나 끝 이외 위치의 `*`는 불러오기 오류입니다.
- **commands**는 argv를 PATH 해석 없이 정확한 문자열로 비교합니다. `cargo`는 `/usr/bin/cargo`와 일치하지 않고, Windows에서 `cargo`는 `cargo.exe`와 일치하지 않습니다. 규칙이 잡 argv의 앞부분과 일치하면 허용됩니다. 컨테이너 잡에도 적용됩니다.
- 와일드카드는 `..` 경로 구성 요소가 들어간 값과 절대 일치하지 않습니다.
- **environments**는 환경 정의(리비전 라벨 제외)의 SHA-256으로 고정하며, 워커가 실제로 받은 내용으로 계산합니다. `setup`, `verify`, `env`, `image`를 고치면 고정이 깨집니다. `sha256: "*"`는 일치하는 ID의 어떤 내용이든 신뢰하므로, 어떤 요청자든 워커에서 setup 셸 명령을 실행할 수 있게 됩니다. 거부 메시지는 붙여 넣을 정확한 `{id, sha256}`을 출력합니다. 프리셋 `uses: rust@1.96.0`은 ID `rust-1.96.0`이 되며, 프리셋 다이제스트는 새 ylcc 릴리스에서 바뀔 수 있습니다. `name:`이 없는 프로젝트의 인라인 환경은 체크아웃 경로에 따라 ID가 달라집니다.
- `self_update: false`는 `ylcc workers update`를 거부합니다.

### 적용 방식

워커는 환경을 준비하거나 이미지를 받거나 입력을 내려받기 전에 각 잡을 검사하며, 거부된 잡은 그 이유와 함께 실패합니다. 정책이 생기기 전에 준비된 환경은 시작할 때 다시 검사해 허용되지 않으면 실패로 표시합니다. 허브는 각 워커가 광고한 정책을 읽어 그 잡을 허용하는 워커에만 배정하고, 등록된 워커 중 허용하는 곳이 없으면 제출 단계에서 거부합니다. 워커가 거부한 잡(예: 이전 허브를 쓰는 경우)은 다른 워커로 다시 배정되지 않습니다. 요구 조건 확인용 `<tool> --version`은 계속 실행됩니다.

### 정책 파일 보호

잡은 워커 계정으로 실행되므로, 정책 파일은 그 계정이 쓸 수 없어야 합니다. 파일은 root나 관리자 소유로 두세요. Unix에서 워커는 그룹이나 다른 사용자가 쓸 수 있는 파일이나 디렉터리를 거부하고, 파일이 자기 계정 소유이면 경고합니다. Windows ACL은 검사하지 않습니다.

### 검사와 제안

```bash
ylcc policy check /etc/ylcc/policy.yaml
ylcc policy suggest --task build --task test
ylcc policy suggest --environment rust@1.96.0 --os linux
```

`check`는 워커와 똑같이 파일을 해석하고 범주별로 제한 없음, 모두 거부, 제한됨 상태와 경고를 출력합니다. `suggest`는 `ylcc.yaml`(다른 경로는 `--file`)을 읽어 해당 태스크(기본값은 전체)와 프리셋만 정확히 허용하는 정책을 출력하며, 모든 범주를 명시적으로 나열합니다. 경로에 따라 달라지는 환경 ID에는 `--allow-path-scoped`가 필요합니다. ylcc를 업그레이드한 뒤에는 다시 실행하세요.

### 정책이 하지 않는 것

허용 목록은 어떤 프로그램이 시작되는지를 제한할 뿐, 그 프로그램이 하는 일은 제한하지 않습니다. `cargo build`, `make`, `npm`, `pytest`는 입력에 들어 있는 코드(`build.rs`, Makefile, 패키지 스크립트, `conftest.py`)를 실행합니다. `sh`, `python` 같은 인터프리터를 허용하면 모든 것을 허용하는 셈입니다. `NODE_OPTIONS`, `PYTHONPATH`, `RUSTC_WRAPPER`, `BASH_ENV` 같은 잡 환경 변수도 코드를 실행할 수 있습니다. 정책은 신뢰하는 운영자 사이의 심층 방어 수단이지 샌드박스가 아닙니다. 격리가 필요하면 컨테이너 이미지를 쓰세요. 명령은 제한했지만 `env`는 제한하지 않았거나, 환경을 고정하지 않았거나, 컨테이너를 제한하지 않았거나, 규칙이 인터프리터를 시작하면 워커는 (거부하지 않고) 경고합니다. 대시보드는 이런 워커를 **Restricted** 대신 **Restricted (bypassable)**로 표시합니다.

## 보호하지 않는 것

- 테넌트 격리가 없습니다. 요청자는 모든 잡, 로그, 출력물, 블롭을 볼 수 있고, 워커 토큰은 해시로 어떤 블롭이든 읽을 수 있으며, 캐시는 한 워커의 잡끼리 공유됩니다.
- 요청자는 정책이 없는 모든 워커, 그리고 정책이 빌드 도구를 허용하는 모든 워커에서 임의의 코드를 실행할 수 있습니다.
- 허브 자체에는 TLS가 없습니다. 비공개 HTTPS 엔드포인트 뒤에 두세요.
- 네이티브 잡은 워커 계정의 권한으로 실행됩니다. 관리자 권한이 없는 전용 계정을 쓰세요([서비스](services.md)).
