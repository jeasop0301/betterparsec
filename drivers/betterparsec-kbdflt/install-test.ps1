[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateSet('Install', 'Uninstall')]
    [string]$Action = 'Install',
    [string]$DriverSys = "$PSScriptRoot\out\x64\betterparsec-kbdflt.sys",
    [string]$BrokerExe = "$PSScriptRoot\..\..\target\release\input-broker.exe",
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'
$driverService = 'BetterParsecKbdFlt'
$brokerService = 'BetterParsecInput'
$classKey = 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4D36E96B-E325-11CE-BFC1-08002BE10318}'
$driverDestination = Join-Path $env:SystemRoot 'System32\drivers\betterparsec-kbdflt.sys'
$brokerDirectory = Join-Path $env:ProgramFiles 'BetterParsec\Input'
$brokerDestination = Join-Path $brokerDirectory 'input-broker.exe'
$backupDirectory = Join-Path $env:ProgramData 'BetterParsec'
$backupPath = Join-Path $backupDirectory 'keyboard-upperfilters.json'

function Resolve-UpperFilterOrder([string[]]$Current) {
    $preserved = @($Current | Where-Object { $_ -ine $driverService })
    $kbdclass = @($preserved | Where-Object { $_ -ieq 'kbdclass' })
    if ($kbdclass.Count -ne 1) {
        throw "Expected exactly one kbdclass entry; found $($kbdclass.Count)."
    }
    $kbdclassIndex = [Array]::FindIndex(
        [string[]]$preserved,
        [Predicate[string]]{ param($item) $item -ieq 'kbdclass' }
    )
    $ordered = @()
    if ($kbdclassIndex -gt 0) {
        $ordered += $preserved[0..($kbdclassIndex - 1)]
    }
    $ordered += $driverService
    $ordered += $preserved[$kbdclassIndex..($preserved.Count - 1)]
    return [string[]]$ordered
}

if ($SelfTest) {
    $cases = @(
        @{ Input = @('kbdclass'); Expected = @($driverService, 'kbdclass') },
        @{ Input = @('VendorA', 'kbdclass', 'VendorB'); Expected = @('VendorA', $driverService, 'kbdclass', 'VendorB') },
        @{ Input = @($driverService, 'kbdclass'); Expected = @($driverService, 'kbdclass') }
    )
    foreach ($case in $cases) {
        $actual = @(Resolve-UpperFilterOrder $case.Input)
        if (($actual -join "`0") -cne ($case.Expected -join "`0")) {
            throw "UpperFilters ordering self-test failed: $($actual -join ', ')."
        }
    }
    Write-Host 'UpperFilters ordering self-test passed.'
    return
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell window.'
}

function Get-UpperFilters {
    $value = (Get-ItemProperty -Path $classKey -Name UpperFilters -ErrorAction Stop).UpperFilters
    return @($value | ForEach-Object { [string]$_ })
}

function Set-UpperFilters([string[]]$Value) {
    Set-ItemProperty -Path $classKey -Name UpperFilters -Type MultiString -Value $Value
}

function Remove-ServiceIfPresent([string]$Name) {
    & sc.exe query $Name *> $null
    if ($LASTEXITCODE -eq 0) {
        & sc.exe stop $Name *> $null
        & sc.exe delete $Name | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "Failed to delete service $Name." }
    }
}

if ($Action -eq 'Uninstall') {
    if ($PSCmdlet.ShouldProcess('BetterParsec input services and keyboard filter', 'Uninstall')) {
        Remove-ServiceIfPresent $brokerService
        $filters = Get-UpperFilters | Where-Object { $_ -ine $driverService }
        if (-not ($filters | Where-Object { $_ -ieq 'kbdclass' })) {
            throw 'Refusing to write UpperFilters without kbdclass.'
        }
        Set-UpperFilters $filters
        Remove-ServiceIfPresent $driverService
        Remove-Item $brokerDestination -Force -ErrorAction SilentlyContinue
        Remove-Item $driverDestination -Force -ErrorAction SilentlyContinue
        Write-Host 'Uninstalled configuration. Reboot is required to unload the keyboard filter.'
    }
    return
}

if (-not (Test-Path -LiteralPath $DriverSys -PathType Leaf)) {
    throw "Driver not found: $DriverSys. Run build-test.cmd first."
}
if (-not (Test-Path -LiteralPath $BrokerExe -PathType Leaf)) {
    throw "Broker not found: $BrokerExe. Run cargo build -p input-broker --release first."
}
$signature = Get-AuthenticodeSignature -LiteralPath $DriverSys
if ($signature.Status -ne 'Valid') {
    throw "Driver signature is $($signature.Status). Sign the SYS with a trusted test certificate before installation."
}

$filters = Get-UpperFilters
$newFilters = @(Resolve-UpperFilterOrder $filters)

if ($PSCmdlet.ShouldProcess('BetterParsec test input driver and LocalSystem broker', 'Install')) {
    New-Item -ItemType Directory -Path $backupDirectory -Force | Out-Null
    if (-not (Test-Path -LiteralPath $backupPath)) {
        $filters | ConvertTo-Json | Set-Content -LiteralPath $backupPath -Encoding UTF8
    }
    Copy-Item -LiteralPath $DriverSys -Destination $driverDestination -Force
    New-Item -ItemType Directory -Path $brokerDirectory -Force | Out-Null
    Copy-Item -LiteralPath $BrokerExe -Destination $brokerDestination -Force

    Remove-ServiceIfPresent $brokerService
    Remove-ServiceIfPresent $driverService
    & sc.exe create $driverService type= kernel start= demand error= normal binPath= $driverDestination | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'Failed to create the keyboard-filter service.' }
    $brokerCommand = '"{0}" --service' -f $brokerDestination
    & sc.exe create $brokerService type= own start= auto obj= LocalSystem binPath= $brokerCommand | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'Failed to create the input-broker service.' }

    Set-UpperFilters $newFilters
    Write-Host ('UpperFilters: ' + ($newFilters -join ', '))
    Write-Host 'Install configured. Reboot is required; the broker starts automatically afterward.'
}
