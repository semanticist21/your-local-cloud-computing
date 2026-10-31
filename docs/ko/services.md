[English](../services.md) | 한국어

# 백그라운드 서비스

워크로드를 실행할 계정으로 설치하세요. 공용 관리 머신에서는 관리자 권한이 없는 전용 계정을 쓰고, 관리용 자격 증명은 그 계정에 두지 마세요. 그 계정의 `HOME`, 토큰, 실행 파일/지원 체크아웃, 도구 캐시, 워커 데이터는 소유권과 권한을 강제하는 파일 시스템에 있어야 합니다. 토큰은 모드 `600`, 비공개 디렉터리는 `700`으로 두고, 실행 파일 경로는 관련 없는 계정이 쓸 수 없게 하세요.

```bash
ylcc --hub https://HUB.example.ts.net:10000 --token-file "$HOME/.config/ylcc/token" \
  service install --role worker --id linux-build --data-dir "$HOME/.local/share/ylcc/worker"

# On the hub machine, using its own local listen address:
ylcc service install --role hub --listen 127.0.0.1:7443 --data-dir "$HOME/.local/share/ylcc/hub"
```

Linux는 `systemd --user`를 씁니다. `systemctl --user status ylcc-worker`와 `journalctl --user -u ylcc-worker`로 확인하세요. 로그인 없이 시작하려면 관리자가 `loginctl enable-linger SERVICE_USER`로 linger를 켜야 합니다. 셸에서 설치하려면 동작하는 사용자 매니저/세션 버스가 필요합니다.

macOS는 기본적으로 로그인 LaunchAgent를 씁니다. `launchctl print gui/$(id -u)/org.ylcc.worker`로 확인하세요. stdout/stderr 로그는 `$HOME/Library/Logs/ylcc/service-ROLE.log`와 `service-ROLE.err.log`입니다. 사용자가 로그인해야 하고, 외부 데이터 볼륨이 있다면 마운트되어 있어야 합니다. 헤드리스 전용 계정이라면 관리자가 `service install --system-user SERVICE_USER ...`로 그 사용자로 실행되는 LaunchDaemon을 설치할 수 있습니다. 시스템 사용자 로그는 `/private/var/log/ylcc/SERVICE_USER/`에 저장되며, 디렉터리는 root 소유이고 일반 파일은 서비스 사용자 소유의 모드 600입니다. 먼저 사용자 소유의 데이터 디렉터리와 읽을 수 있는 보호된 토큰을 준비하세요.

```bash
sudo /absolute/path/to/ylcc --hub https://HUB.example.ts.net:10000 \
  --token-file /absolute/service-home/.config/ylcc/token service install \
  --system-user SERVICE_USER --role worker --id mac-build \
  --data-dir /absolute/persistent-volume/ylcc/worker
```

서비스 설치는 현재 실행 파일 위치와 네이티브 사용자의 `PATH`를 캡처하므로, 바이너리를 그 위치에 두세요. macOS 시스템 사용자 설치는 사용자별 `HOME`과 표준 도구 경로를 설정합니다. 도구 경로를 바꾼 뒤에는 서비스를 다시 설치하고, 서비스 계정에서 기능을 확인하세요.

워커가 실행할 작업을 제한하려면 `service install --role worker`에 `--policy /absolute/path/policy.yaml`을 추가하세요. 파일은 설치할 때 검증되고, 서비스는 그 절대 경로로 실행됩니다. 파일은 root나 관리자 소유로 두고 서비스 계정이 쓸 수 없게 하세요. macOS `--system-user`를 쓰는 경우 그 계정이 파일을 읽을 수는 있어야 합니다. [워커 명령 정책](security.md#워커-명령-정책)을 참고하세요.

워커는 heartbeat 대신 15초마다 호스트 CPU, 메모리, 1분 load, 데이터 디스크 사용량을 보냅니다. Hub가 7일간 보관하고 대시보드의 워커 페이지에 차트로 보여 줍니다. 보고를 끄려면 `ylcc worker --no-metrics`로 실행하세요.

launchd stdout/stderr 로그는 외부 잡 데이터와 분리해 내부 시스템 볼륨에 두세요. macOS는 실행 파일을 시작하기 전에 이동식 볼륨의 로그 경로 열기를 거부할 수 있으며, 이 경우 워커 로그 없이 `posix_spawn: Operation not permitted`나 `EX_CONFIG`(78)가 납니다. 이 실패를 진단할 때는 plist의 `StandardOutPath`/`StandardErrorPath`와 `launchctl print`를 확인하세요. 외부 토큰을 읽을 수 있고 작업 디렉터리가 유효하다고 해서 launchd가 그 로그 경로를 열 수 있다는 뜻은 아닙니다.

“소유권 무시”(`noowners`)가 켜진 외부 Mac 볼륨은 이 계정 격리를 강제할 수 없습니다. 그 볼륨 위의 평문 sparsebundle로도 해결되지 않습니다. 마운트된 이미지 내부의 권한과 관계없이 다른 로컬 계정이 백킹 밴드를 읽거나 수정할 수 있기 때문입니다.

macOS에서는 관리자가 기존 APFS 컨테이너에 [별도의 APFS 볼륨을 추가](https://support.apple.com/guide/disk-utility/add-delete-or-erase-apfs-volumes-dskua9e6a110/mac)하고(필요하면 할당량 포함), 그 볼륨에 소유권을 켤 수 있습니다. 이렇게 하면 이웃 볼륨의 소유권 설정을 바꾸지 않고 별도 저장소를 둘 수 있습니다. OS 도구로 볼륨을 준비하고 마운트한 뒤 소유권이 강제되는지 확인하고, 서비스 계정의 파일을 그곳에 두세요. 워커는 데이터 볼륨을 쓸 수 있게 된 뒤에만 시작하거나 재시작하세요.

macOS의 [파일 접근 개인정보 보호 제어](https://support.apple.com/en-nz/guide/security/secddd1d86a6/web)는 이동식 볼륨에도 적용됩니다. 서비스 사용자가 모드 `700`으로 디렉터리를 소유하더라도 백그라운드 LaunchDaemon의 접근이 거부될 수 있습니다. CLI가 설치되어 있고, 토큰을 읽을 수 있고, 대화형/SSH 접근이 된다고 해서 백그라운드 서비스의 개인정보 보호 승인이 된 것은 아닙니다. `worker.lock` 생성/열기 거부는 가능한 증상 중 하나입니다. 권한을 바꾸기 전에 워커 오류와 macOS 개인정보 보호 로그를 확인하세요.

이동식 볼륨 접근이 거부된 단독 네이티브 워커의 경우, Mac 관리자가 **시스템 설정 → 개인정보 보호 및 보안 → 전체 디스크 접근 권한**을 열고 **+**를 눌러, 서비스 plist가 쓰는 고정 경로(예: `/Volumes/YLCC/bin/ylcc`)의 설치된 `ylcc` 실행 파일을 추가하고 켤 수 있습니다. 이전 항목이 있는데도 거부된다면 그 항목을 제거하고 실제 실행 파일을 다시 추가하세요. 워커를 재시작하고, 그 LaunchDaemon을 통해 외부 데이터 쓰기와 실제 잡을 확인하세요. macOS 승인 UI를 쓰고, TCC 데이터베이스와 SIP 설정은 건드리지 마세요. 서명되지 않았거나 애드혹 서명된 실행 파일을 업데이트한 뒤에는 실제 서비스 환경에서 접근을 다시 확인하세요. 게시된 macOS 릴리스는 동일한 Developer ID 서명 식별자를 사용해 후속 업데이트에서도 그 식별자를 유지합니다.

사용자 서비스는 `ylcc service uninstall --role worker`(또는 `hub`)로 제거합니다. 토큰과 워크로드 데이터는 유지되므로, 더 이상 필요 없을 때만 명시적으로 지우세요. 시스템 사용자 LaunchDaemon은 관리자가 `service uninstall --role worker --system-user SERVICE_USER`로 제거합니다.

## 워커 업데이트

관리자는 대시보드 워커 목록이나 상세 화면의 **Update to v…** 버튼으로도 업데이트할 수 있습니다. 브라우저를 닫거나 허브를 재시작해도 요청한 버전으로 워커가 재접속할 때까지 새 작업 배정을 보류합니다. 실행 중인 작업은 먼저 끝내며 기존 일시정지 설정을 유지합니다. 실패 이유를 화면에 표시하고 컨테이너·오프라인·정책상 업데이트가 금지된 워커는 제외합니다.

일시정지된 워커에는 관리자 전용 **Resume** 버튼이 목록과 상세 화면에 표시됩니다. 재개하면 저장된 일시정지 설정을 해제합니다. 업데이트 중이라면 완료할 때까지 새 작업 배정은 계속 보류합니다.

```bash
ylcc workers update --all                      # 최신 게시 릴리스, 워커를 하나씩
ylcc workers update mac-build --version v0.5.17
```

현재 허브에서는 CLI도 같은 허브 관리 배정 보류 기능을 사용하며, 워커의 저장된 일시정지 설정을 바꾸지 않습니다. 이전 허브에서는 원래 실행 중이던 워커를 잠시 정지했다가 재개하므로 명령이 끝날 때까지 CLI 연결을 유지하세요. 각 워커는 실행 중인 잡을 마친 뒤, 자기 OS/아키텍처용 릴리스 아카이브를 내려받아 릴리스의 `SHA256SUMS`와 새 바이너리의 `--version`으로 확인하고, 자기 실행 파일을 교체한 다음 상태 75로 종료합니다. systemd(`Restart=on-failure`), launchd(`KeepAlive`), Windows 태스크 러너가 새 바이너리로 다시 시작합니다. 명령은 다음 워커로 넘어가며 실패하면 거기서 멈춥니다. 실행 파일이 있는 디렉터리는 워커 계정이 쓸 수 있어야 하므로, 워커 바이너리는 root 소유 디렉터리가 아니라 그 계정 아래(예: `~/.local/bin`)에 설치하세요. macOS에서는 소유권이 맞아도 외장 볼륨 접근이 거부될 수 있습니다. 위의 전체 디스크 접근 권한 절차를 확인하세요. 컨테이너 워커는 건너뜁니다. 새 릴리스 이미지로 다시 만드세요. 허브는 먼저 허브 호스트에서 `ylcc update --apply`로 올리세요. `workers`가 워커별 `version`을 보여 줍니다.

## 워커 디스크 정리

워커는 설정 없이 일반적인 CI/클러스터 보존 규칙을 따릅니다.

- 시도의 체크아웃, `TMPDIR`/`TMP`/`TEMP` 디렉터리, 전용 환경은 시도가 끝나고 출력이 업로드되면 바로 지웁니다. 저널과 로그는 3일간 남기고, `unknown` 시도는 정리될 때까지 남깁니다.
- 7일 동안 쓰지 않은 입력 블롭, 태스크 캐시, 환경 리비전은 지웁니다. GitHub Actions 캐시 만료와 같은 기준입니다.
- 태스크 캐시가 100GiB를 넘으면 최근 사용 여부와 관계없이 가장 오래된 캐시부터 통째로 지웁니다. 잡 실행 제한이 아니라 정리를 시작하는 기준입니다.
- 데이터 파일 시스템 사용량이 85%를 넘으면, kubelet의 이미지 정리처럼 가장 오래 안 쓴 것부터 80% 아래가 될 때까지 지웁니다. 태스크 캐시가 자체 상한을 넘은 경우를 제외하면 최근 1시간 안에 쓴 것은 보호하므로, 다른 데이터로 디스크가 찼을 때 잡마다 다시 빌드하는 일은 없습니다.
- 카탈로그에서 환경을 삭제하면 연결된 워커의 파일은 즉시 지워지고, 오프라인 워커는 사용 기한으로 정리합니다.

정리는 매시간, 각 시도가 끝난 뒤, 시작할 때 실행됩니다. 디스크 공간이 부족하고 작업 중일 때는 완료된 시도 파일과 사용 중이지 않은 캐시를 정리하면서 새 작업을 받아들입니다. 작업 공간·입력 블롭·환경 정리는 유휴 상태까지 미룹니다. GC 로그에는 잠금 대기 시간, 정리 시간, 실제 파일시스템 회수량을 따로 기록합니다. 지워진 환경은 그 환경이 필요한 다음 잡이 다시 준비합니다.

## 재접속과 전송 한도

새 워커는 작은 Hello와 작업 ID·상태를 먼저 확인한 뒤, Hub에 연속으로 저장된 로그 번호 이후부터 복구하고 바뀐 결과를 따로 보냅니다. 단일 송신기가 각 작업의 로그 다음에 완료 보고를 보내며, 이력 복구 중에도 명령 수신과 heartbeat를 계속합니다. 큰 실행 명세는 다운로드 전에 저널에 기록해 연결 종료·재시작 후 이어받고, 작업 취소 시 다운로드도 중단합니다. 구버전 워커는 업데이트 전까지 기존 접속 방식을 사용합니다.

1MiB보다 큰 JSON 메타데이터는 SHA-256을 확인하는 blob 경로로 전송합니다. 이는 전송 경로를 고르는 기준이며 수락 한도나 전송량 제한이 아닙니다. 큰 결과도 전체를 스트리밍합니다. 일반 파일 본문은 전체 크기 제한 없이 스트리밍합니다.

허브와 워커는 시작할 때 다음 환경변수를 한 번 읽습니다. 값은 양의 정수 MiB 단위입니다(1MiB = 1,048,576바이트). 잘못된 값, 0, 정수 범위를 넘는 값은 변수 이름을 표시하고 시작을 중단합니다. `/health`의 `transfer_limits`에서 허브에 적용된 값을 바이트 단위로 확인할 수 있습니다.

| 환경변수 | 기본값 | 적용 대상 |
| --- | --- | --- |
| `YLCC_WS_MAX_MIB` | 128 | 허브·워커가 받는 WebSocket 메시지와 프레임 |
| `YLCC_HTTP_JSON_MAX_MIB` | 128 | 허브가 받는 HTTP JSON 요청 본문 |
| `YLCC_METADATA_MAX_MIB` | 512 | 허브·워커가 해석하거나 보내는 직렬화된 메타데이터 한 건 |

예를 들어 WebSocket·HTTP JSON은 256MiB, 메타데이터는 1GiB로 설정하려면:

```bash
export YLCC_WS_MAX_MIB=256
export YLCC_HTTP_JSON_MAX_MIB=256
export YLCC_METADATA_MAX_MIB=1024
ylcc --token-file /absolute/path/token hub --data-dir /absolute/path/hub
# 각 워커 프로세스 환경에도 같은 값을 설정한 뒤 시작합니다.
```

실제 서비스 환경에 설정해야 합니다. systemd는 `Environment=` override, launchd는 `EnvironmentVariables`, Windows는 작업을 실행하는 환경, 컨테이너는 `environment` 또는 `-e`를 사용합니다. 대화형 셸에서 export해도 이미 설치·실행된 서비스에는 적용되지 않으며, 설정을 바꾼 서비스는 재시작해야 합니다. 양쪽은 메타데이터 수신 한도를 알려 주고, 송신에는 로컬·상대 한도 중 작은 값을 적용합니다. 한도를 알리지 않는 구버전은 256MiB로 취급합니다. 실행·환경 준비·완료 메타데이터가 한도를 넘으면 해당 작업만 명시적으로 실패시킵니다. WebSocket 수신 한도는 로컬 설정이므로 풀 전체의 값을 맞춰 주세요. 구버전은 기존 고정 한도를 유지합니다. 메타데이터 한도에는 작업 명세·환경 둘과 JSON 포장까지 담을 여유가 필요합니다. JSON 한도를 높이면 동시 전송 수만큼 메모리 사용이 늘 수 있으며, 해석된 JSON은 직렬화된 바이트보다 많은 메모리를 차지할 수 있습니다.

완료 메타데이터가 한도를 넘으면 명시적인 작업 실패로 보고하고 원본 결과는 정상 보관 기간 동안 워커 저널에 남깁니다. 개별 로그 본문 한도는 1MiB이며 워커는 8KiB씩 나눠 보냅니다. 구버전 워커의 16MiB 프레임 한도를 넘는 실행 명세는 해당 작업만 업데이트 안내와 함께 실패시키고 워커 연결은 유지합니다.
