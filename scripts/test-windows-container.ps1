# Runs only in the CI test image, as ContainerUser with an initialized dedicated volume.
$ErrorActionPreference = 'Stop'
$ylcc = 'C:\ylcc\ylcc.exe'
$tokenFile = 'C:\ylcc-data\config\token'
$base = Join-Path 'C:\ylcc-data' ('test-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($base) | Out-Null
$results = Join-Path $base 'results'
$nativeError = Join-Path $base 'cli.err'
$hubProcess = $null
$workerProcess = $null
function Invoke-Ylcc([string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    $output = & $ylcc @connection @Arguments 2> $nativeError
    $code = $LASTEXITCODE
    if ($code -ne 0) { throw "YLCC failed ($code): $([IO.File]::ReadAllText($nativeError))" }
    return ($output | Out-String | ConvertFrom-Json)
}
function Wait-Until([scriptblock]$Check, [string]$Description) {
    foreach ($attempt in 1..120) {
        $value = & $Check
        if ($value) { return $value }
        Start-Sleep -Milliseconds 500
    }
    throw "Timed out waiting for $Description."
}
try {
    $ErrorActionPreference = 'Continue'
    & $ylcc --version
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "ylcc.exe cannot start in Server Core (exit $code); inspect runtime DLL requirements." }
    $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $hub = "http://127.0.0.1:$port"
    $connection = @('--hub', $hub, '--token-file', $tokenFile, '--results-dir', $results, '--json')
    $hubProcess = Start-Process -FilePath $ylcc -ArgumentList @('--token-file', $tokenFile, 'hub', '--listen', "127.0.0.1:$port", '--data-dir', (Join-Path $base 'hub')) -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $base 'hub.log') -RedirectStandardError (Join-Path $base 'hub.err')
    Wait-Until {
        try { (Invoke-WebRequest -UseBasicParsing -Uri "$hub/health").StatusCode -eq 200 } catch { $false }
    } 'container-local Hub' | Out-Null
    $workerProcess = Start-Process -FilePath $ylcc -ArgumentList @('--hub', $hub, '--token-file', $tokenFile, 'worker', '--id', 'windows-container-test', '--data-dir', 'C:\ylcc-data\worker', '--max-jobs', '1') -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $base 'worker.log') -RedirectStandardError (Join-Path $base 'worker.err')
    $worker = Wait-Until {
        @(Invoke-Ylcc -Arguments @('workers')) | Where-Object { $_.id -eq 'windows-container-test' -and $_.online }
    } 'ContainerUser worker connection'
    if ($worker.capabilities.os -ne 'windows' -or $worker.capabilities.arch -ne 'x86_64' -or -not $worker.capabilities.containerized) { throw 'The worker did not report Windows x86_64 container capabilities.' }
    Set-Location -LiteralPath $base
    $source = '$ErrorActionPreference="Stop"; $sum=0; 1..100 | ForEach-Object { $sum += $_ * $_ }; @{sum=$sum} | ConvertTo-Json | Set-Content -LiteralPath result.json -Encoding UTF8'
    [IO.File]::WriteAllText((Join-Path $base 'input.ps1'), $source)
    $job = Invoke-Ylcc -Arguments @('run', '--input', 'input.ps1', '--os', 'windows', '--arch', 'x86_64', '--output', 'result.json', '--', 'powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', 'input.ps1')
    if ($job.state -ne 'succeeded') { throw 'The Windows container job did not succeed.' }
    $answer = Get-Content -LiteralPath (Join-Path $results ($job.id + '\result.json')) -Raw | ConvertFrom-Json
    if ($answer.sum -ne 338350) { throw 'The fetched result was incorrect.' }
    if ([IO.File]::ReadAllText((Join-Path $base 'input.ps1')) -cne $source) { throw 'The requester source was changed.' }

    $cancelScript = '$child=Start-Process powershell.exe -ArgumentList "-NoProfile -NonInteractive -Command Start-Sleep -Seconds 60" -PassThru; Write-Output ("childpid="+$child.Id); Start-Sleep -Seconds 60'
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cancelScript))
    $cancel = Invoke-Ylcc -Arguments @('run', '--no-input', '--os', 'windows', '--arch', 'x86_64', '--detach', '--', 'powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded)
    $childPid = Wait-Until {
        $logs = @(Invoke-Ylcc -Arguments @('jobs', 'logs', $cancel.id))
        $text = ($logs | ForEach-Object { $_.text }) -join ''
        if ($text -match 'childpid=(\d+)') { [int]$Matches[1] }
    } 'owned child process'
    Invoke-Ylcc -Arguments @('jobs', 'cancel', $cancel.id) | Out-Null
    Wait-Until { (Invoke-Ylcc -Arguments @('jobs', 'show', $cancel.id)).state -eq 'cancelled' } 'cancelled job state' | Out-Null
    Wait-Until { -not (Get-Process -Id $childPid -ErrorAction SilentlyContinue) } 'owned child process termination' | Out-Null
    @{sum=$answer.sum; job=$job.id; attempt=$job.attempt_id; inputHash=$job.spec.inputs[0].hash; cancellation='passed'; worker='windows-container-test'} | ConvertTo-Json | Set-Content -LiteralPath 'C:\ylcc-data\proof.json' -Encoding UTF8
    Write-Host 'Windows container checks passed: ContainerUser, initialized token, real job, fetched result, source preservation, and descendant cancellation.'
} catch {
    foreach ($name in @('hub.err', 'worker.err', 'cli.err')) {
        $path = Join-Path $base $name
        if (Test-Path -LiteralPath $path) { Get-Content -LiteralPath $path -Tail 20 | Write-Host }
    }
    throw
} finally {
    foreach ($process in @($workerProcess, $hubProcess)) {
        if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
    }
}
