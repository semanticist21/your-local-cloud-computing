# Host-side checks for a disposable Windows process-isolated image and named volume.
[CmdletBinding()]
param(
    [string]$Image = 'ylcc-windows:test',
    [string]$RuntimeImage = 'ylcc-windows:runtime',
    [string]$Binary
)
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($Binary)) { $Binary = Join-Path $PSScriptRoot '..\dist\ylcc.exe' }
function Invoke-Docker([string[]]$Arguments, [switch]$ExpectFailure) {
    $ErrorActionPreference = 'Continue'
    & docker @Arguments
    $code = $LASTEXITCODE
    if ($ExpectFailure) {
        if ($code -eq 0) { throw 'An unsafe Windows volume initializer was accepted.' }
    } elseif ($code -ne 0) { throw "Docker failed with exit code $code." }
}
$volume = 'ylcc-windows-test-' + [Guid]::NewGuid().ToString('N')
$taskTemp = Join-Path ([IO.Path]::GetTempPath()) $volume
$bootstrap = Join-Path $taskTemp 'bootstrap'
[IO.Directory]::CreateDirectory($bootstrap) | Out-Null
try {
    & $Binary --token-file (Join-Path $bootstrap 'pool.token') init
    if ($LASTEXITCODE -ne 0) { throw 'Failed to create the disposable test pool token.' }
    $originalTokenHash = (Get-FileHash -LiteralPath (Join-Path $bootstrap 'pool.token') -Algorithm SHA256).Hash
    $identityProbe = '$ProgressPreference="SilentlyContinue"; $i=[Security.Principal.WindowsIdentity]::GetCurrent(); @{sid=$i.User.Value; name=$i.Name; admin=(New-Object Security.Principal.WindowsPrincipal($i)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)} | ConvertTo-Json -Compress'
    $encodedIdentity = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($identityProbe))
    $workerIdentity = & docker run --rm --isolation=process --user ContainerUser --entrypoint powershell.exe $RuntimeImage -NoProfile -NonInteractive -EncodedCommand $encodedIdentity
    if ($LASTEXITCODE -ne 0) { throw 'Cannot discover the runtime ContainerUser identity.' }
    $workerIdentity = $workerIdentity | Out-String | ConvertFrom-Json
    if ($workerIdentity.admin -ne $false -or $workerIdentity.name -notmatch '(^|\\)ContainerUser$') { throw 'The runtime did not provide an unprivileged ContainerUser identity.' }
    $workerSid = $workerIdentity.sid
    Invoke-Docker -Arguments @('volume', 'create', $volume)
    $mounts = @('--mount', "type=volume,source=$volume,target=C:\ylcc-data")
    $initialize = @('run', '--rm', '--isolation=process', '--user', 'ContainerAdministrator') + $mounts + @('--mount', "type=bind,source=$bootstrap,target=C:\ylcc-bootstrap,readonly", '--entrypoint', 'powershell.exe', $RuntimeImage, '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', 'C:\ylcc\Initialize-WindowsWorker.ps1', '-WorkerSid', $workerSid)
    foreach ($unsafeSid in @('S-1-1-0', 'S-1-5-18')) {
        $unsafeInitialize = $initialize.Clone()
        $unsafeInitialize[-1] = $unsafeSid
        Invoke-Docker -Arguments $unsafeInitialize -ExpectFailure
    }
    Invoke-Docker -Arguments $initialize
    Invoke-Docker -Arguments $initialize # Same-token initialization must preserve an existing volume.
    $volumeAdmin = @('run', '--rm', '--isolation=process', '--user', 'ContainerAdministrator') + $mounts + @('--entrypoint', 'powershell.exe', $RuntimeImage, '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand')
    $linkCommand = '$ErrorActionPreference="Stop"; New-Item -ItemType HardLink -Path C:\ylcc-data\config\token.link -Target C:\ylcc-data\config\token | Out-Null'
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($linkCommand))
    Invoke-Docker -Arguments ($volumeAdmin + @($encoded))
    try { Invoke-Docker -Arguments $initialize -ExpectFailure }
    finally {
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('Remove-Item -LiteralPath C:\ylcc-data\config\token.link -ErrorAction Stop'))
        Invoke-Docker -Arguments ($volumeAdmin + @($encoded))
    }
    Invoke-Docker -Arguments $initialize
    Invoke-Docker -Arguments (@('run', '--rm', '--isolation=process') + $mounts + @($Image))
    $proof = @'
$ErrorActionPreference = 'Stop'
$proof = Get-Content -LiteralPath C:\ylcc-data\proof.json -Raw | ConvertFrom-Json
if ($proof.sum -ne 338350 -or $proof.cancellation -ne 'passed') { throw 'Persistent worker proof was lost.' }
$attempt = Join-Path C:\ylcc-data\worker\attempts $proof.attempt
$journal = Get-Content -LiteralPath (Join-Path $attempt 'journal.json') -Raw | ConvertFrom-Json
if ($journal.report.job_id -ne $proof.job -or $journal.report.state -ne 'succeeded') { throw 'The completed attempt journal was lost.' }
$inputBlob = Join-Path C:\ylcc-data\worker\blobs $proof.inputHash
if ((Get-FileHash -LiteralPath $inputBlob -Algorithm SHA256).Hash -ne $proof.inputHash) { throw 'The cached input was lost or changed.' }
if (Test-Path -LiteralPath (Join-Path $attempt 'work')) { throw 'The finished attempt checkout was not released.' }
Write-Host 'Recreated Windows container retained the completed journal and input cache and released the checkout.'
'@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($proof))
    Invoke-Docker -Arguments (@('run', '--rm', '--isolation=process') + $mounts + @('--entrypoint', 'powershell.exe', $RuntimeImage, '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded))
    # Refuse token replacement without modifying the retained volume or its job data.
    Remove-Item -LiteralPath (Join-Path $bootstrap 'pool.token')
    & $Binary --token-file (Join-Path $bootstrap 'pool.token') init
    if ($LASTEXITCODE -ne 0) { throw 'Failed to create the distinct test token.' }
    Invoke-Docker -Arguments $initialize -ExpectFailure
    $unchanged = 'if((Get-FileHash -LiteralPath C:\ylcc-data\config\token -Algorithm SHA256).Hash -ne "' + $originalTokenHash + '"){throw "Existing pool token changed"}; ' + $proof
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($unchanged))
    Invoke-Docker -Arguments (@('run', '--rm', '--isolation=process') + $mounts + @('--entrypoint', 'powershell.exe', $RuntimeImage, '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded))
    Write-Host 'Windows image checks passed: one-time initialization, idempotence, ContainerUser workload, cancellation, volume recreation, and token replacement refusal.'
} finally {
    try { Invoke-Docker -Arguments @('volume', 'rm', $volume) }
    finally { if (Test-Path -LiteralPath $taskTemp) { Remove-Item -LiteralPath $taskTemp -Recurse -Force } }
}
