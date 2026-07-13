param(
    [Parameter(Mandatory = $true)]
    [string] $StageRoot,
    [Parameter(Mandatory = $true)]
    [string] $TestResultPath,
    [Parameter(Mandatory = $true)]
    [string] $CleanupResultPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'This helper must run elevated.'
}

$test = Get-Content -Raw -LiteralPath $TestResultPath | ConvertFrom-Json
$foundationExe = Join-Path $StageRoot 'sunshine.exe'
Get-CimInstance Win32_Process |
    Where-Object { $_.Name -eq 'sunshine.exe' -and $_.ExecutablePath -eq $foundationExe } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force }

Start-Service -Name SunshineService
(Get-Service -Name SunshineService).WaitForStatus('Running', [TimeSpan]::FromSeconds(20))
Start-Sleep -Seconds 3

$hashMatches = @{}
foreach ($property in $test.StockHashesBefore.PSObject.Properties) {
    $current = (Get-FileHash -LiteralPath (Join-Path 'C:\Program Files\Sunshine\config' $property.Name) -Algorithm SHA256).Hash
    $hashMatches[$property.Name] = ($current -eq $property.Value)
}

$portOwner = (Get-NetTCPConnection -State Listen -LocalPort 47989 -ErrorAction Stop | Select-Object -First 1).OwningProcess
$stockProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$portOwner"
$success = (Get-Service SunshineService).Status -eq 'Running' -and
    $stockProcess.ExecutablePath -eq 'C:\Program Files\Sunshine\sunshine.exe' -and
    -not ($hashMatches.Values -contains $false)

if ($success) {
    Set-Content -LiteralPath $test.CompletionSentinel -Value (Get-Date).ToUniversalTime().ToString('o') -Encoding UTF8
}

$watchdogStopped = $false
if ($test.WatchdogProcessId) {
    $watchdog = Get-Process -Id $test.WatchdogProcessId -ErrorAction SilentlyContinue
    if ($watchdog) {
        Stop-Process -Id $watchdog.Id -Force -ErrorAction SilentlyContinue
    }
    $watchdogStopped = -not (Get-Process -Id $test.WatchdogProcessId -ErrorAction SilentlyContinue)
}

[pscustomobject]@{
    Success = $success
    ServiceStatus = (Get-Service SunshineService).Status.ToString()
    StockProcessId = $portOwner
    StockExecutable = $stockProcess.ExecutablePath
    StockHashMatches = $hashMatches
    FoundationStopped = -not (Get-Process -Id $test.ProcessId -ErrorAction SilentlyContinue)
    WatchdogStopped = $watchdogStopped
    CompletedAt = (Get-Date).ToUniversalTime().ToString('o')
} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $CleanupResultPath -Encoding UTF8

if (-not $success) {
    throw 'Stock Sunshine restoration verification failed.'
}
