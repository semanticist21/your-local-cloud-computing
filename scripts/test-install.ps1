# Offline installation checks. Run with Windows PowerShell 5.1 or PowerShell 7 on Windows.
[CmdletBinding()]
param(
    [string]$Installer,
    [switch]$SkipExecutionPolicyCheck
)
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($Installer)) { $Installer = Join-Path $PSScriptRoot 'install.ps1' }
if (-not $SkipExecutionPolicyCheck) {
    # Reproduce a Restricted client without changing any persistent policy scope.
    $policyTemp = Join-Path ([IO.Path]::GetTempPath()) ('ylcc-policy-test-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($policyTemp) | Out-Null
    $probe = Join-Path $policyTemp 'unsigned probe.ps1'
    $marker = Join-Path $policyTemp 'executed'
    [IO.File]::WriteAllText($probe, "[IO.File]::WriteAllText('" + $marker.Replace("'", "''") + "', 'executed')")
    $restrictedCommand = @'
$ErrorActionPreference = 'Stop'
if ((Get-ExecutionPolicy -Scope Process) -ne 'Restricted') { throw 'Restricted parent was not established.' }
$before = Get-ExecutionPolicy -List | Where-Object { $_.Scope -ne 'Process' } | ConvertTo-Json -Compress
$ErrorActionPreference = 'Continue'
& (Join-Path $PSHOME 'powershell.exe') -NoProfile -NonInteractive -File __PROBE__ 2>$null
$blockedCode = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($blockedCode -eq 0 -or (Test-Path -LiteralPath __MARKER__)) { throw 'Restricted parent allowed an unsigned script without child Bypass.' }
& (Join-Path $PSHOME 'powershell.exe') -NoProfile -NonInteractive -ExecutionPolicy Bypass -File __TEST__ -Installer __INSTALLER__ -SkipExecutionPolicyCheck
if ($LASTEXITCODE -ne 0) { throw 'Child-process Bypass installer checks failed.' }
if ((Get-ExecutionPolicy -Scope Process) -ne 'Restricted') { throw 'The child changed its parent process policy.' }
$after = Get-ExecutionPolicy -List | Where-Object { $_.Scope -ne 'Process' } | ConvertTo-Json -Compress
if ($before -cne $after) { throw 'A persistent execution policy scope changed.' }
Write-Host 'Restricted-parent check passed: only the child process used Bypass; persistent policies are unchanged.'
'@
    foreach ($replacement in @{
        '__PROBE__' = $probe
        '__MARKER__' = $marker
        '__TEST__' = $PSCommandPath
        '__INSTALLER__' = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Installer)
    }.GetEnumerator()) {
        $restrictedCommand = $restrictedCommand.Replace($replacement.Key, "'" + $replacement.Value.Replace("'", "''") + "'")
    }
    try {
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($restrictedCommand))
        $windowsPowerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        & $windowsPowerShell -NoProfile -NonInteractive -ExecutionPolicy Restricted -EncodedCommand $encoded
        if ($LASTEXITCODE -ne 0) { throw 'Restricted-parent installation checks failed.' }
    } finally { Remove-Item -LiteralPath $policyTemp -Recurse -Force }
    return
}
if ((Get-ExecutionPolicy -Scope Process) -ne 'Bypass') { throw 'Installer checks require process-local Bypass.' }
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$taskTemp = Join-Path ([IO.Path]::GetTempPath()) ('ylcc-install-test-' + [Guid]::NewGuid().ToString('N'))
$release = Join-Path $taskTemp 'release'
$prefix = Join-Path $taskTemp "prefix with spaces and ' quote"
$archive = 'ylcc-windows-x86_64.zip'
[IO.Directory]::CreateDirectory($release) | Out-Null
function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function New-Fixture([string]$Variant = 'valid') {
    $path = Join-Path $release $archive
    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    $zip = [IO.Compression.ZipFile]::Open($path, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($name in @('ylcc.exe', 'LICENSE', 'THIRD_PARTY_NOTICES.txt')) {
            $entry = $zip.CreateEntry($name)
            if ($name -eq 'ylcc.exe' -and $Variant -eq 'symlink') { $entry.ExternalAttributes = -1577123840 }
            if ($name -eq 'ylcc.exe' -and $Variant -eq 'directory') { $entry.ExternalAttributes = 0x10 }
            $writer = New-Object IO.StreamWriter($entry.Open())
            try { $writer.Write("fixture:$name") } finally { $writer.Dispose() }
        }
        switch ($Variant) {
            'extra' { $zip.CreateEntry('extra') | Out-Null }
            'duplicate' { $zip.CreateEntry('ylcc.exe') | Out-Null }
            'traversal' { $zip.CreateEntry('../escaped') | Out-Null }
            'case' { $zip.CreateEntry('YLCC.EXE') | Out-Null }
        }
    } finally { $zip.Dispose() }
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $release 'SHA256SUMS'), "$hash  $archive`n")
}
# This intercepts downloads, keeping the actual installer and its ZIP/hash handling under test.
function Invoke-WebRequest {
    param([switch]$UseBasicParsing, [string]$Uri, [string]$OutFile)
    $base = 'https://github.com/semanticist21/your-local-cloud-computing/releases/download/v0.5.9/'
    Assert ($Uri.StartsWith($base)) "Unexpected release URL: $Uri"
    [IO.File]::Copy((Join-Path $release $Uri.Substring($base.Length)), $OutFile, $true)
}
function Assert-Rejected([string]$Variant) {
    $failed = $false
    try { & $Installer -Version v0.5.9 -Prefix $prefix } catch { $failed = $true }
    Assert $failed "Unsafe fixture accepted: $Variant"
    Assert ([IO.File]::ReadAllText((Join-Path $prefix 'bin\ylcc.exe')) -eq 'fixture:ylcc.exe') "Existing executable changed: $Variant"
    Assert (-not (Test-Path -LiteralPath (Join-Path $taskTemp 'escaped'))) 'Archive traversal escaped the staging directory.'
}
try {
    New-Fixture
    & $Installer -Version v0.5.9 -Prefix $prefix
    Assert ([IO.File]::ReadAllText((Join-Path $prefix 'bin\ylcc.exe')) -eq 'fixture:ylcc.exe') 'Executable did not install.'
    Assert ([IO.File]::ReadAllText((Join-Path $prefix 'share\licenses\ylcc\LICENSE')) -eq 'fixture:LICENSE') 'License did not install.'
    & $Installer -Version v0.5.9 -Prefix $prefix
    [IO.File]::AppendAllText((Join-Path $release $archive), 'corruption')
    Assert-Rejected 'checksum mismatch'
    foreach ($variant in @('extra', 'duplicate', 'traversal', 'case', 'symlink', 'directory')) {
        New-Fixture $variant
        Assert-Rejected $variant
    }
    New-Fixture
    $manifest = Join-Path $release 'SHA256SUMS'
    [IO.File]::AppendAllText($manifest, [IO.File]::ReadAllText($manifest))
    Assert-Rejected 'duplicate checksum'
    Write-Host 'Windows installer checks passed: checksum, exact members, links, traversal, spaces, replacement, and license notices.'
} finally {
    Remove-Item -LiteralPath $taskTemp -Recurse -Force
}
