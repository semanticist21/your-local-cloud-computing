English | [한국어](ko/windows.md)

# Windows

## Windows containers

The Windows worker image is a **Windows/amd64 Server Core LTSC 2022 container**. It requires a Docker engine in Windows-container mode and a compatible Windows host. The existing Linux image stays separate. Windows images are pending publication; until a Windows image has passed CI and is published, build the source runtime below. Versioned Windows image tags use `VERSION-windows-ltsc2022`, separately from the Linux amd64/arm64 tag.

Microsoft supports the LTSC 2022 image on Windows Server 2022/2025 and Windows 11 with the appropriate isolation mode; consult the [host/image compatibility matrix](https://learn.microsoft.com/en-us/virtualization/windowscontainers/deploy-containers/version-compatibility) and [Windows container prerequisites](https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/set-up-environment). Confirm the selected daemon before installing:

```powershell
docker info --format '{{.OSType}}'  # Must print windows.
```

The image includes YLCC and Windows PowerShell 5.1. Workload tools such as Java, Android command-line SDKs and compilers belong in a custom image. Windows containers support headless commands; they do not provide an Android Studio desktop or an Android Emulator/GUI testing environment. Keep simulator or desktop testing on an appropriate native worker. See Microsoft's [Windows container application limits](https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/lift-shift-to-containers).

From a Windows support checkout, build the binary as described in [Windows CLI and worker](#windows-cli-and-worker), then prepare and build the image:

```powershell
New-Item -ItemType Directory -Path dist -Force | Out-Null
Copy-Item target/release/ylcc.exe,LICENSE,THIRD_PARTY_NOTICES.txt dist/
docker build --file Dockerfile.windows --target runtime --tag ylcc-windows:local .
$env:YLCC_WINDOWS_IMAGE = 'ylcc-windows:local'
```

Use a **dedicated named volume** for worker jobs, environments and the protected token copy. Transfer your existing pool token through the secure administration channel into a private host directory as `pool.token`. Set `$bootstrap` to that directory; the initializer mounts it read-only for this one-time operation. Discover the image's actual unprivileged ContainerUser identity before initialization; the initializer protects only the fixed YLCC volume paths and refuses to replace a different existing token. It never changes the host token's ACL or prints its contents.

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

The initializer is the only step run as ContainerAdministrator. The ongoing worker runs as **ContainerUser** using only the named volume. Its private directories/token allow ContainerUser, SYSTEM and built-in Administrators; they are owned by the built-in Administrators group. No host secret directory is mounted into the ongoing worker. Source snapshots, writable attempts, logs, environment caches and returned files persist under `C:\ylcc-data\worker` when the container is recreated.

Configure the actual private HTTPS Hub URL, a unique worker ID and concurrency, then start the [Windows Compose example](../compose.windows.yaml):

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

The two DNS overrides are optional. Leave them unset when the container's default DNS can resolve the Hub. `100.100.100.100` is Tailscale's device-local [Quad100 DNS resolver](https://tailscale.com/docs/reference/quad100); keep a regular resolver as fallback so public hostnames used by jobs still resolve.

Docker's restart policy manages the worker while the Windows container engine is running. Set up engine startup through your existing Windows runtime. Request `--os windows --arch x86_64` for these jobs; this worker advertises Windows and `containerized: true`. The plain image has no host runtime pipe or nested container engine, and Windows GPU capabilities are not advertised without a working native probe.

## Windows CLI and worker

Releases include `ylcc-windows-x86_64.zip`. Native Windows x86_64 is supported; Windows arm64 release artifacts and a Windows Hub background service are not provided, so keep the Hub on Linux/macOS. Windows uses the same outward HTTPS/WebSocket connection and shared token; SSH, WSL and Docker are not required.

To build from source, install [Rust's MSVC toolchain](https://rust-lang.org/tools/install/) and Visual Studio Build Tools with the C++ desktop workload and Windows SDK (SQLite is bundled). In PowerShell, from this checkout:

```powershell
cargo build --locked --release
$ylcc = (Resolve-Path .\target\release\ylcc.exe).Path
& $ylcc --version
```

The installer detects x86_64, checks `SHA256SUMS` and validates the ZIP before installing to `$env:LOCALAPPDATA\Programs\ylcc\bin`. Replace `RELEASE_TAG` below with a published tag:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install.ps1 -Version RELEASE_TAG
```

`-ExecutionPolicy Bypass` applies only to that child process; it does not change your user or machine execution policy. Organization Group Policy can still restrict execution. The installer places the executable and licenses without changing PATH. Keep the executable at its installed location after registering a background worker; upgrade it with `ylcc workers update` (see [Background services](services.md#updating-workers)).

Transfer your existing pool token through your secure administration channel to `$env:APPDATA\ylcc\token`. Restrict its ACL to your user, SYSTEM and Administrators; `ylcc init` creates a protected token only when setting up a **new** pool. Do not generate a different token on a joining worker. Then configure and start:

```powershell
$token = Join-Path $env:APPDATA 'ylcc\token'
$data = Join-Path $env:LOCALAPPDATA 'ylcc\worker'
& $ylcc --hub https://HUB.example.ts.net:10000 --token-file $token configure
& $ylcc --json doctor
& $ylcc worker --id windows-build --data-dir $data --max-jobs 1
```

After confirming the foreground connection, stop it with Ctrl+C and install the background worker from the same configured user shell:

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

This is a Task Scheduler task running as the installing user with limited privileges and an interactive logon token. It starts at that user's logon and requires the user to stay logged in; a locked session is fine. It is not a Windows SCM service and does not start before login. For a machine that must serve jobs after a reboot without anyone logging in, add `--at-startup` and run the install from an elevated PowerShell: the task then starts at boot as an S4U task, still unelevated as the same user, but without network credentials (SMB shares and other Windows-authenticated resources are unavailable; the Hub connection is unaffected). Local Task Scheduler policy can reject registration; the command reports the error. The task captures the installing shell's PATH. Reinstall it after changing SDK/tool paths, and check `workers` for the actual service capabilities. Use a local ACL-capable disk such as NTFS for private token/job data; filesystems without working ACL protection are rejected.

From the support checkout, submit the native [PowerShell example](../examples/jobs/windows-check.ps1) from any requester:

```bash
ylcc run --input examples/jobs/windows-check.ps1 --os windows --arch x86_64 \
  --output result.json -- powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File examples/jobs/windows-check.ps1
```

On Windows PowerShell, put that command on one line instead of using Bash's `\` continuation. Request `windows` explicitly for Windows tools; a Linux container worker on a Windows host advertises Linux. Android builds/emulator jobs additionally require the Android SDK, Java/Gradle and emulator virtualization already configured for the worker user. Run Gradle with `--no-daemon` so the job owns its processes instead of reusing an external daemon. YLCC does not install those tools or provide interactive Android Studio remote desktop control. A build test does not establish emulator or real-device support on your particular PC.
