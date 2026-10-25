# Windows PowerShell 5.1+; run from a normal user shell.
[CmdletBinding()]
param(
    [ValidatePattern('^v[0-9][A-Za-z0-9._-]*$')]
    [string]$Version = 'v0.5.4',
    [string]$Prefix = $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA 'Programs\ylcc' } else { '' })
)
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'This installer requires Windows.' }
try { $architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() }
catch { $architecture = $(if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }) }
if ($architecture -notin @('X64', 'AMD64')) { throw "Prebuilt Windows binaries support x86_64 only (detected $architecture). Native Windows arm64 is not released." }
if ([string]::IsNullOrWhiteSpace($Prefix)) { throw 'LOCALAPPDATA is unavailable; supply -Prefix with an absolute path.' }
$Prefix = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Prefix)
if ($Prefix.StartsWith('\\')) { throw 'Install the Windows executable on a local disk, not a network share.' }
$archive = 'ylcc-windows-x86_64.zip'
$base = "https://github.com/semanticist21/your-local-cloud-computing/releases/download/$Version"
$taskTemp = Join-Path ([IO.Path]::GetTempPath()) ('ylcc-install-' + [Guid]::NewGuid().ToString('N'))
$installTemp = $null
$zip = $null
try {
    [IO.Directory]::CreateDirectory($taskTemp) | Out-Null
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    foreach ($name in @($archive, 'SHA256SUMS')) {
        try { Invoke-WebRequest -UseBasicParsing -Uri "$base/$name" -OutFile (Join-Path $taskTemp $name) }
        catch { throw "Cannot download $name from release $Version. Choose a published release that includes Windows x86_64. $($_.Exception.Message)" }
    }
    $checksumLines = @(Get-Content -LiteralPath (Join-Path $taskTemp 'SHA256SUMS') | Where-Object { $_ -match ('^[0-9a-fA-F]{64}\s+\*?' + [Regex]::Escape($archive) + '$') })
    if ($checksumLines.Count -ne 1) { throw 'Archive missing or duplicated in SHA256SUMS.' }
    $expected = $checksumLines[0].Substring(0, 64)
    $actual = (Get-FileHash -LiteralPath (Join-Path $taskTemp $archive) -Algorithm SHA256).Hash
    if ($actual -ne $expected) { throw 'Checksum mismatch: refusing installation.' }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead((Join-Path $taskTemp $archive))
    $names = @('ylcc.exe', 'LICENSE', 'THIRD_PARTY_NOTICES.txt')
    if ($zip.Entries.Count -ne $names.Count) { throw 'Unexpected archive contents.' }
    foreach ($name in $names) {
        $entries = @($zip.Entries | Where-Object { $_.FullName -ceq $name })
        if ($entries.Count -ne 1) { throw 'Unexpected or duplicated archive member.' }
        $entry = $entries[0]
        $unixType = ($entry.ExternalAttributes -shr 16) -band 0xF000
        if (($unixType -ne 0 -and $unixType -ne 0x8000) -or ($entry.ExternalAttributes -band 0x10)) { throw 'Archive members must be regular files.' }
        $inputStream = $entry.Open()
        $outputStream = [IO.File]::Create((Join-Path $taskTemp $name))
        try { $inputStream.CopyTo($outputStream) }
        finally { $outputStream.Dispose(); $inputStream.Dispose() }
    }
    $zip.Dispose(); $zip = $null
    $bin = Join-Path $Prefix 'bin'
    $notices = Join-Path $Prefix 'share\licenses\ylcc'
    [IO.Directory]::CreateDirectory($bin) | Out-Null
    [IO.Directory]::CreateDirectory($notices) | Out-Null
    # Stage on the destination filesystem. A running Windows executable must be stopped before replacement.
    $installTemp = Join-Path $bin ('.ylcc-' + [Guid]::NewGuid().ToString('N') + '.exe')
    [IO.File]::Copy((Join-Path $taskTemp 'ylcc.exe'), $installTemp)
    $destination = Join-Path $bin 'ylcc.exe'
    if ([IO.File]::Exists($destination)) {
        try { [IO.File]::Replace($installTemp, $destination, [System.Management.Automation.Language.NullString]::Value) }
        catch { throw "Cannot replace ylcc.exe. Drain active jobs, uninstall/stop the worker task, close other YLCC processes, and retry. $($_.Exception.Message)" }
    } else { [IO.File]::Move($installTemp, $destination) }
    $installTemp = $null
    foreach ($name in @('LICENSE', 'THIRD_PARTY_NOTICES.txt')) { [IO.File]::Copy((Join-Path $taskTemp $name), (Join-Path $notices $name), $true) }
    Write-Host "Installed $Version to $destination"
    Write-Host "Add $bin to your user PATH, or run it by its full path. No PATH or service setting was changed."
} finally {
    if ($zip) { $zip.Dispose() }
    if ($installTemp -and (Test-Path -LiteralPath $installTemp)) { Remove-Item -LiteralPath $installTemp -Force }
    if (Test-Path -LiteralPath $taskTemp) { Remove-Item -LiteralPath $taskTemp -Recurse -Force }
}
