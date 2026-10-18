[English](../windows.md) | 한국어

# Windows

## Windows 컨테이너

Windows 워커 이미지는 **Windows/amd64 Server Core LTSC 2022 컨테이너**입니다. Windows 컨테이너 모드의 Docker 엔진과 호환되는 Windows 호스트가 필요합니다. 기존 Linux 이미지는 별도로 유지됩니다. Windows 이미지는 아직 게시 전입니다. Windows 이미지가 CI를 통과해 게시되기 전까지는 아래의 소스 런타임을 빌드하세요. 버전이 붙은 Windows 이미지 태그는 Linux amd64/arm64 태그와 별도로 `VERSION-windows-ltsc2022` 형식을 씁니다.

Microsoft는 적절한 격리 모드에서 Windows Server 2022/2025와 Windows 11에서 LTSC 2022 이미지를 지원합니다. [호스트/이미지 호환성 표](https://learn.microsoft.com/en-us/virtualization/windowscontainers/deploy-containers/version-compatibility)와 [Windows 컨테이너 사전 요구 사항](https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/set-up-environment)을 확인하세요. 설치 전에 선택된 데몬을 확인합니다.

```powershell
docker info --format '{{.OSType}}'  # Must print windows.
```

이미지에는 YLCC와 Windows PowerShell 5.1이 들어 있습니다. Java, Android 명령줄 SDK, 컴파일러 같은 워크로드 도구는 커스텀 이미지에 넣으세요. Windows 컨테이너는 헤드리스 명령을 지원하며, Android Studio 데스크톱이나 Android Emulator/GUI 테스트 환경은 제공하지 않습니다. 시뮬레이터나 데스크톱 테스트는 적절한 네이티브 워커에서 하세요. Microsoft의 [Windows 컨테이너 애플리케이션 제약](https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/lift-shift-to-containers)을 참고하세요.

Windows 지원 체크아웃에서 [Windows CLI와 워커](#windows-cli와-워커)에 설명된 대로 바이너리를 빌드한 뒤, 이미지를 준비하고 빌드합니다.

```powershell
New-Item -ItemType Directory -Path dist -Force | Out-Null
Copy-Item target/release/ylcc.exe,LICENSE,THIRD_PARTY_NOTICES.txt dist/
docker build --file Dockerfile.windows --target runtime --tag ylcc-windows:local .
$env:YLCC_WINDOWS_IMAGE = 'ylcc-windows:local'
```

워커 잡, 환경, 보호된 토큰 사본에는 **전용 named volume**을 쓰세요. 기존 풀 토큰을 안전한 관리 채널로 비공개 호스트 디렉터리에 `pool.token`으로 옮깁니다. `$bootstrap`을 그 디렉터리로 지정하세요. 초기화 스크립트는 이 일회성 작업 동안만 해당 디렉터리를 읽기 전용으로 마운트합니다. 초기화 전에 이미지의 실제 비특권 ContainerUser ID를 확인하세요. 초기화 스크립트는 정해진 YLCC 볼륨 경로만 보호하며, 이미 있는 다른 토큰은 덮어쓰지 않습니다. 호스트 토큰의 ACL을 바꾸거나 내용을 출력하지도 않습니다.

```powershell
$env:YLCC_WINDOWS_DATA_VOLUME = 'ylcc-windows-worker'
$env:YLCC_WINDOWS_ISOLATION = 'hyperv'  # Choose a supported mode for your host.
$bootstrap = 'C:\path\to\private-token-folder'
$identityProbe = '$ProgressPreference="SilentlyContinue"; $i=[Security.Principal.WindowsIdentity]::GetCurrent(); @{sid=$i.User.Value; name=$i.Name; admin=(New-Object Security.Principal.WindowsPrincipal($i)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)} | ConvertTo-Json -Compress'
$encodedIdentity = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($identityProbe))
$workerIdentity = docker run --rm --isolation $env:YLCC_WINDOWS_ISOLATION --user ContainerUser --entrypoint powershell.exe $env:YLCC_WINDOWS_IMAGE -NoProfile -NonInteractive -EncodedCommand $encodedIdentity
if ($LASTEXITCODE -ne 0) { throw 'Cannot discover the image ContainerUser identity.' }
$workerIdentity = $workerIdentity | Out-String | ConvertFrom-Json
if ($workerIdentity.admin -ne $false -or $workerIdentity.name -notmatch '(^|\\)ContainerUser$') { throw 'Expected the unprivileged ContainerUser identity.' }
docker volume create $env:YLCC_WINDOWS_DATA_VOLUME
docker run --rm --isolation $env:YLCC_WINDOWS_ISOLATION --user ContainerAdministrator --mount "type=volume,source=$env:YLCC_WINDOWS_DATA_VOLUME,target=C:\ylcc-data" --mount "type=bind,source=$bootstrap,target=C:\ylcc-bootstrap,readonly" --entrypoint powershell.exe $env:YLCC_WINDOWS_IMAGE -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\ylcc\Initialize-WindowsWorker.ps1 -WorkerSid $workerIdentity.sid
```

ContainerAdministrator로 실행되는 단계는 초기화 스크립트뿐입니다. 이후 계속 도는 워커는 named volume만 사용하는 **ContainerUser**로 실행됩니다. 워커의 비공개 디렉터리와 토큰은 ContainerUser, SYSTEM, 기본 제공 Administrators에게만 허용되며, 소유자는 기본 제공 Administrators 그룹입니다. 계속 도는 워커에는 호스트의 비밀 디렉터리가 마운트되지 않습니다. 소스 스냅샷, 쓰기 가능한 시도, 로그, 환경 캐시, 반환 파일은 컨테이너를 다시 만들어도 `C:\ylcc-data\worker` 아래에 남습니다.

실제 비공개 HTTPS 허브 URL, 고유한 워커 ID, 동시 실행 수를 설정한 뒤 [Windows Compose 예제](../../compose.windows.yaml)를 시작합니다.

```powershell
$env:YLCC_HUB_URL = 'https://HUB.example.ts.net:10000'
$env:YLCC_WORKER_ID = 'windows-container'
$env:YLCC_MAX_JOBS = '1'
# For a tailnet-only Hub, use MagicDNS plus your usual public DNS resolver.
$env:YLCC_WINDOWS_DNS_PRIMARY = '100.100.100.100'
$env:YLCC_WINDOWS_DNS_FALLBACK = 'YOUR_STANDARD_DNS_SERVER'
docker compose -f compose.windows.yaml up -d
docker compose -f compose.windows.yaml logs --tail 40
# Removing the container retains the external named volume:
docker compose -f compose.windows.yaml down
```

두 DNS 재정의는 선택 사항입니다. 컨테이너의 기본 DNS로 허브 주소를 해석할 수 있다면 설정하지 마세요. `100.100.100.100`은 Tailscale의 기기 로컬 [Quad100 DNS 리졸버](https://tailscale.com/docs/reference/quad100)입니다. 잡이 쓰는 공개 호스트 이름도 해석되도록 일반 리졸버를 폴백으로 두세요.

Windows 컨테이너 엔진이 실행 중인 동안에는 Docker의 재시작 정책이 워커를 관리합니다. 엔진 시작은 기존 Windows 런타임 설정으로 구성하세요. 이 잡들에는 `--os windows --arch x86_64`를 요청하세요. 이 워커는 Windows와 `containerized: true`를 광고합니다. 기본 이미지에는 호스트 런타임 파이프나 중첩 컨테이너 엔진이 없으며, 실제로 동작하는 네이티브 감지가 없으면 Windows GPU 기능도 광고하지 않습니다.

## Windows CLI와 워커

릴리스에는 `ylcc-windows-x86_64.zip`이 포함됩니다. 네이티브 Windows x86_64를 지원하며, Windows arm64 릴리스 아티팩트와 Windows 허브 백그라운드 서비스는 제공하지 않으므로 허브는 Linux/macOS에 두세요. Windows도 같은 바깥 방향 HTTPS/WebSocket 연결과 공유 토큰을 쓰며, SSH, WSL, Docker는 필요하지 않습니다.

소스에서 빌드하려면 [Rust MSVC 툴체인](https://rust-lang.org/tools/install/)과, C++ 데스크톱 워크로드 및 Windows SDK가 포함된 Visual Studio Build Tools를 설치하세요(SQLite는 번들로 포함됩니다). PowerShell에서 이 체크아웃 위치에서:

```powershell
cargo build --locked --release
$ylcc = (Resolve-Path .\target\release\ylcc.exe).Path
& $ylcc --version
```

설치 스크립트는 x86_64를 감지하고 `SHA256SUMS`를 확인하며 ZIP을 검증한 뒤 `$env:LOCALAPPDATA\Programs\ylcc\bin`에 설치합니다. 아래 `RELEASE_TAG`를 게시된 태그로 바꾸세요.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install.ps1 -Version RELEASE_TAG
```

`-ExecutionPolicy Bypass`는 그 자식 프로세스에만 적용되며 사용자나 머신의 실행 정책을 바꾸지 않습니다. 조직의 그룹 정책은 여전히 실행을 제한할 수 있습니다. 설치 스크립트는 PATH를 바꾸지 않고 실행 파일과 라이선스만 배치합니다. 백그라운드 워커를 등록한 뒤에는 실행 파일을 설치된 위치에 그대로 두고, 업그레이드는 `ylcc workers update`로 하세요([백그라운드 서비스](services.md#워커-업데이트) 참고).

기존 풀 토큰이나 `ylcc tokens create --role worker --worker-id windows-build`로 만든 워커 토큰([보안](security.md#토큰))을 안전한 관리 채널로 `$env:APPDATA\ylcc\token`에 옮깁니다. ACL은 본인 사용자, SYSTEM, Administrators로 제한하세요. `ylcc init`은 **새** 풀을 설정할 때만 보호된 토큰을 만듭니다. 합류하는 워커에서 다른 토큰을 생성하지 마세요. 그다음 설정하고 시작합니다.

```powershell
$token = Join-Path $env:APPDATA 'ylcc\token'
$data = Join-Path $env:LOCALAPPDATA 'ylcc\worker'
& $ylcc --hub https://HUB.example.ts.net:10000 --token-file $token configure
& $ylcc --json doctor
& $ylcc worker --id windows-build --data-dir $data --max-jobs 1
```

포그라운드 연결을 확인했으면 Ctrl+C로 멈추고, 같은 설정의 사용자 셸에서 백그라운드 워커를 설치합니다.

```powershell
& $ylcc service install --role worker --id windows-build --data-dir $data --max-jobs 1
& $ylcc --json workers
# Inspect only this user's task and service log:
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
Get-ScheduledTask -TaskName "YLCC-worker-$sid"
Get-Content -LiteralPath (Join-Path $env:LOCALAPPDATA 'ylcc\service\worker.log') -Tail 40
# Remove automatic startup; tokens, logs and workload data are retained:
& $ylcc service uninstall --role worker
```

이것은 설치한 사용자로, 제한된 권한과 대화형 로그온 토큰으로 실행되는 작업 스케줄러 태스크입니다. 그 사용자가 로그온할 때 시작되며, 사용자가 계속 로그인되어 있어야 합니다. 세션이 잠겨 있는 것은 괜찮습니다. Windows SCM 서비스가 아니므로 로그인 전에는 시작되지 않습니다. 재부팅 후 아무도 로그인하지 않아도 잡을 받아야 하는 머신이라면 `--at-startup`을 붙여 관리자 권한 PowerShell에서 설치하세요. 그러면 태스크가 부팅 시 S4U 태스크로 시작하며, 같은 사용자의 일반 권한으로 실행되지만 네트워크 자격 증명은 없습니다(SMB 공유 등 Windows 인증이 필요한 리소스는 쓸 수 없고, 허브 연결에는 영향이 없습니다). 로컬 작업 스케줄러 정책이 등록을 거부할 수 있으며, 그 경우 명령이 오류를 보고합니다. 태스크는 설치한 셸의 PATH를 캡처합니다. SDK/도구 경로를 바꾼 뒤에는 다시 설치하고, `workers`로 서비스의 실제 기능을 확인하세요. 비공개 토큰/잡 데이터에는 NTFS처럼 ACL을 지원하는 로컬 디스크를 쓰세요. ACL 보호가 제대로 동작하지 않는 파일 시스템은 거부됩니다.

[워커 명령 정책](security.md#워커-명령-정책)은 Windows에서도 argv를 정확히 비교합니다. `cargo` 규칙은 `cargo.exe`나 전체 경로로 실행하는 잡과 일치하지 않으므로, 잡이 명령을 쓰는 방식 그대로 규칙을 작성하세요. 워커는 정책 파일의 ACL을 검사하지 않습니다. 워커 사용자가 수정할 수 없는 Administrators 소유 위치에 두세요.

지원 체크아웃에서 아무 요청자에서나 네이티브 [PowerShell 예제](../../examples/jobs/windows-check.ps1)를 제출합니다.

```bash
ylcc run --input examples/jobs/windows-check.ps1 --os windows --arch x86_64 \
  --output result.json -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File examples/jobs/windows-check.ps1
```

Windows PowerShell에서는 Bash의 `\` 줄 이어 쓰기 대신 이 명령을 한 줄로 쓰세요. Windows 도구가 필요하면 `windows`를 명시적으로 요청하세요. Windows 호스트의 Linux 컨테이너 워커는 Linux를 광고합니다. Android 빌드/에뮬레이터 잡에는 추가로 워커 사용자에게 Android SDK, Java/Gradle, 에뮬레이터 가상화가 미리 구성되어 있어야 합니다. 잡이 외부 데몬을 재사용하지 않고 자기 프로세스를 소유하도록 Gradle은 `--no-daemon`으로 실행하세요. YLCC는 이런 도구를 설치하지 않으며, Android Studio 원격 데스크톱 대화형 제어도 제공하지 않습니다. 빌드 테스트가 통과했다고 해서 특정 PC에서 에뮬레이터나 실제 기기 지원이 확인되는 것은 아닙니다.
