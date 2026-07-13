Set-StrictMode -Version Latest

$script:BenchmarkIsWindows = [Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [Runtime.InteropServices.OSPlatform]::Windows
)

function Test-BenchmarkAdministrator {
    if (-not $script:BenchmarkIsWindows) {
        return $false
    }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Write-BenchmarkJson {
    param(
        [Parameter(Mandatory = $true)] $Value,
        [Parameter(Mandatory = $true)][string] $Path,
        [int] $Depth = 12
    )

    $directory = Split-Path -Parent $Path
    if ($directory) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $Value | ConvertTo-Json -Depth $Depth | Set-Content -LiteralPath $Path -Encoding UTF8
}

function Get-BenchmarkGitSnapshot {
    param([Parameter(Mandatory = $true)][string] $Repository)

    $head = (& git -C $Repository rev-parse HEAD 2>$null)
    $branch = (& git -C $Repository branch --show-current 2>$null)
    $status = @(& git -C $Repository status --short 2>$null)
    return [ordered]@{
        head = if ($LASTEXITCODE -eq 0 -or $head) { "$head".Trim() } else { 'unknown' }
        branch = if ($branch) { "$branch".Trim() } else { 'detached-or-unknown' }
        dirty = $status.Count -gt 0
        status = $status
    }
}

function Get-BenchmarkNetworkSnapshot {
    if (-not $script:BenchmarkIsWindows) {
        return @()
    }

    return @(Get-NetAdapterStatistics -ErrorAction SilentlyContinue | ForEach-Object {
        [ordered]@{
            name = $_.Name
            receivedBytes = [uint64]$_.ReceivedBytes
            sentBytes = [uint64]$_.SentBytes
            receivedUnicastPackets = [uint64]$_.ReceivedUnicastPackets
            sentUnicastPackets = [uint64]$_.SentUnicastPackets
            receivedDiscardedPackets = [uint64]$_.ReceivedDiscardedPackets
            outboundDiscardedPackets = [uint64]$_.OutboundDiscardedPackets
            receivedPacketErrors = [uint64]$_.ReceivedPacketErrors
            outboundPacketErrors = [uint64]$_.OutboundPacketErrors
        }
    })
}

function Get-BenchmarkNetworkDelta {
    param(
        [Parameter(Mandatory = $true)] $Before,
        [Parameter(Mandatory = $true)] $After
    )

    $beforeByName = @{}
    foreach ($adapter in $Before) {
        $beforeByName[$adapter.name] = $adapter
    }

    return @($After | ForEach-Object {
        $current = $_
        $previous = $beforeByName[$current.name]
        if (-not $previous) {
            return
        }
        [ordered]@{
            name = $current.name
            receivedBytes = [uint64]([Math]::Max(0, [double]$current.receivedBytes - [double]$previous.receivedBytes))
            sentBytes = [uint64]([Math]::Max(0, [double]$current.sentBytes - [double]$previous.sentBytes))
            totalBytes = [uint64]([Math]::Max(0, ([double]$current.receivedBytes + [double]$current.sentBytes) - ([double]$previous.receivedBytes + [double]$previous.sentBytes)))
            receivedUnicastPackets = [uint64]([Math]::Max(0, [double]$current.receivedUnicastPackets - [double]$previous.receivedUnicastPackets))
            sentUnicastPackets = [uint64]([Math]::Max(0, [double]$current.sentUnicastPackets - [double]$previous.sentUnicastPackets))
            receivedDiscardedPackets = [uint64]([Math]::Max(0, [double]$current.receivedDiscardedPackets - [double]$previous.receivedDiscardedPackets))
            outboundDiscardedPackets = [uint64]([Math]::Max(0, [double]$current.outboundDiscardedPackets - [double]$previous.outboundDiscardedPackets))
            receivedPacketErrors = [uint64]([Math]::Max(0, [double]$current.receivedPacketErrors - [double]$previous.receivedPacketErrors))
            outboundPacketErrors = [uint64]([Math]::Max(0, [double]$current.outboundPacketErrors - [double]$previous.outboundPacketErrors))
        }
    })
}

function Get-BenchmarkSystemSnapshot {
    param([Parameter(Mandatory = $true)][string] $Repository)

    $os = if ($script:BenchmarkIsWindows) {
        Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue
    } else {
        $null
    }
    $cpu = if ($script:BenchmarkIsWindows) {
        Get-CimInstance Win32_Processor -ErrorAction SilentlyContinue | Select-Object -First 1
    } else {
        $null
    }
    $gpus = if ($script:BenchmarkIsWindows) {
        @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | ForEach-Object {
            [ordered]@{
                name = $_.Name
                driverVersion = $_.DriverVersion
                videoProcessor = $_.VideoProcessor
                adapterRam = $_.AdapterRAM
            }
        })
    } else {
        @()
    }

    return [ordered]@{
        capturedAt = (Get-Date).ToUniversalTime().ToString('o')
        computerName = [Environment]::MachineName
        userName = [Environment]::UserName
        os = [ordered]@{
            caption = $os.Caption
            version = $os.Version
            buildNumber = $os.BuildNumber
            architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        }
        cpu = [ordered]@{
            name = $cpu.Name
            cores = $cpu.NumberOfCores
            logicalProcessors = $cpu.NumberOfLogicalProcessors
        }
        gpus = $gpus
        powershell = $PSVersionTable.PSVersion.ToString()
        rustc = ((& rustc -V 2>$null) -join '').Trim()
        cargo = ((& cargo -V 2>$null) -join '').Trim()
        node = ((& node -v 2>$null) -join '').Trim()
        git = Get-BenchmarkGitSnapshot -Repository $Repository
    }
}

function Get-BenchmarkTraceDurationSeconds {
    param([Parameter(Mandatory = $true)] $Trace)

    $total = 0.0
    foreach ($phase in $Trace.phases) {
        $total += [double]$phase.durationSeconds
    }
    return $total
}

function Invoke-BenchmarkExternal {
    param(
        [Parameter(Mandatory = $true)][string] $FilePath,
        [string[]] $ArgumentList = @(),
        [switch] $DryRun
    )

    $display = "$FilePath $($ArgumentList -join ' ')".Trim()
    if ($DryRun) {
        Write-Host "[dry-run] $display"
        return
    }

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: $display"
    }
}
