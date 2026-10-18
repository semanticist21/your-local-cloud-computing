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

서비스 설치는 현재 실행 파일 위치와 네이티브 사용자의 `PATH`를 캡처하므로, 바이너리를 그 위치에 두세요. macOS 시스템 사용자 설치는 사용자별 `HOME`과 표준 도구 경로를 설정합니다. 도구 경로를 바꾼 뒤에는 서비스를 다시 설치하고, 서비스 계정에서 기능을 확인하세요. 업데이트는 진행 중인 잡이 끝난 뒤 워커를 재시작해야 합니다. 잡 실행 중에 환경을 교체하지 마세요.

launchd stdout/stderr 로그는 외부 잡 데이터와 분리해 내부 시스템 볼륨에 두세요. macOS는 실행 파일을 시작하기 전에 이동식 볼륨의 로그 경로 열기를 거부할 수 있으며, 이 경우 워커 로그 없이 `posix_spawn: Operation not permitted`나 `EX_CONFIG`(78)가 납니다. 이 실패를 진단할 때는 plist의 `StandardOutPath`/`StandardErrorPath`와 `launchctl print`를 확인하세요. 외부 토큰을 읽을 수 있고 작업 디렉터리가 유효하다고 해서 launchd가 그 로그 경로를 열 수 있다는 뜻은 아닙니다.

“소유권 무시”(`noowners`)가 켜진 외부 Mac 볼륨은 이 계정 격리를 강제할 수 없습니다. 그 볼륨 위의 평문 sparsebundle로도 해결되지 않습니다. 마운트된 이미지 내부의 권한과 관계없이 다른 로컬 계정이 백킹 밴드를 읽거나 수정할 수 있기 때문입니다.

macOS에서는 관리자가 기존 APFS 컨테이너에 [별도의 APFS 볼륨을 추가](https://support.apple.com/guide/disk-utility/add-delete-or-erase-apfs-volumes-dskua9e6a110/mac)하고(필요하면 할당량 포함), 그 볼륨에 소유권을 켤 수 있습니다. 이렇게 하면 이웃 볼륨의 소유권 설정을 바꾸지 않고 별도 저장소를 둘 수 있습니다. OS 도구로 볼륨을 준비하고 마운트한 뒤 소유권이 강제되는지 확인하고, 서비스 계정의 파일을 그곳에 두세요. 워커는 데이터 볼륨을 쓸 수 있게 된 뒤에만 시작하거나 재시작하세요.

macOS의 [파일 접근 개인정보 보호 제어](https://support.apple.com/en-nz/guide/security/secddd1d86a6/web)는 이동식 볼륨에도 적용됩니다. 서비스 사용자가 모드 `700`으로 디렉터리를 소유하더라도 백그라운드 LaunchDaemon의 접근이 거부될 수 있습니다. CLI가 설치되어 있고, 토큰을 읽을 수 있고, 대화형/SSH 접근이 된다고 해서 백그라운드 서비스의 개인정보 보호 승인이 된 것은 아닙니다. `worker.lock` 생성/열기 거부는 가능한 증상 중 하나입니다. 권한을 바꾸기 전에 워커 오류와 macOS 개인정보 보호 로그를 확인하세요.

이동식 볼륨 접근이 거부된 단독 네이티브 워커의 경우, Mac 관리자가 **시스템 설정 → 개인정보 보호 및 보안 → 전체 디스크 접근 권한**을 열고 **+**를 눌러, 서비스 plist가 쓰는 고정 경로(예: `/usr/local/bin/ylcc`)의 설치된 `ylcc` 실행 파일을 추가하고 켤 수 있습니다. 워커를 재시작하고, 그 LaunchDaemon을 통해 외부 데이터 쓰기와 실제 잡을 확인하세요. macOS 승인 UI를 쓰고, TCC 데이터베이스와 SIP 설정은 건드리지 마세요. 서명되지 않았거나 애드혹 서명된 실행 파일을 업데이트한 뒤에는 실제 서비스 환경에서 접근을 다시 확인하세요.

사용자 서비스는 `ylcc service uninstall --role worker`(또는 `hub`)로 제거합니다. 토큰과 워크로드 데이터는 유지되므로, 더 이상 필요 없을 때만 명시적으로 지우세요. 시스템 사용자 LaunchDaemon은 관리자가 `service uninstall --role worker --system-user SERVICE_USER`로 제거합니다.
