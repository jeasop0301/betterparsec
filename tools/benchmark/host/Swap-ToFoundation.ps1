param(
    [Parameter(Mandatory = $true)]
    [string] $StageRoot,
    [Parameter(Mandatory = $true)]
    [string] $LauncherScript,
    [Parameter(Mandatory = $true)]
    [string] $RestoreScript,
    [Parameter(Mandatory = $true)]
    [string] $ResultPath,
    [Parameter(Mandatory = $true)]
    [string] $CompletionSentinel,
    [int] $Port = 47989
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'This helper must run elevated.'
}

$foundationExe = Join-Path $StageRoot 'sunshine.exe'
$stockConfig = 'C:\Program Files\Sunshine\config'
$trackedFiles = @(
    'apps.json',
    'sunshine.conf',
    'sunshine_state.json',
    'credentials\cacert.pem',
    'credentials\cakey.pem'
)
$stockHashes = @{}
foreach ($file in $trackedFiles) {
    $path = Join-Path $stockConfig $file
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Tracked stock Sunshine file is missing: $path"
    }
    $stockHashes[$file] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
}

Remove-Item -LiteralPath $CompletionSentinel -Force -ErrorAction SilentlyContinue

Get-CimInstance Win32_Process |
    Where-Object { $_.Name -eq 'sunshine.exe' -and $_.ExecutablePath -eq $foundationExe } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force }

Stop-Service -Name SunshineService -Force
(Get-Service -Name SunshineService).WaitForStatus('Stopped', [TimeSpan]::FromSeconds(20))

try {
    & $LauncherScript -StageRoot $StageRoot -ResultPath $ResultPath -Port $Port
    $launch = Get-Content -Raw -LiteralPath $ResultPath | ConvertFrom-Json
    Start-Sleep -Seconds 7

    [void](Get-Process -Id $launch.ProcessId -ErrorAction Stop)
    $listener = Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction Stop |
        Where-Object { $_.OwningProcess -eq $launch.ProcessId }
    if (-not $listener) {
        throw "Foundation did not own TCP port $Port after startup."
    }

    $pwsh = (Get-Command pwsh.exe -ErrorAction Stop).Source
    $watchdogArgs = @(
        '-NoProfile',
        '-ExecutionPolicy', 'Bypass',
        '-File', ('"' + $RestoreScript + '"'),
        '-FoundationExe', ('"' + $foundationExe + '"'),
        '-CompletionSentinel', ('"' + $CompletionSentinel + '"'),
        '-DelaySeconds', '600'
    )
    $watchdog = Start-Process -FilePath $pwsh -ArgumentList $watchdogArgs -WindowStyle Hidden -PassThru

    $launch | Add-Member -NotePropertyName StockServiceStopped -NotePropertyValue $true
    $launch | Add-Member -NotePropertyName StockHashesBefore -NotePropertyValue $stockHashes
    $launch | Add-Member -NotePropertyName WatchdogProcessId -NotePropertyValue $watchdog.Id
    $launch | Add-Member -NotePropertyName CompletionSentinel -NotePropertyValue $CompletionSentinel
    $launch | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $ResultPath -Encoding UTF8
}
catch {
    Get-CimInstance Win32_Process |
        Where-Object { $_.Name -eq 'sunshine.exe' -and $_.ExecutablePath -eq $foundationExe } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Start-Service -Name SunshineService -ErrorAction SilentlyContinue
    throw
}
