[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('BetterParsec', 'ParsecWeb', 'ParsecNative', 'Custom')]
    [string] $Candidate,

    [string] $ProfilePath,
    [string] $TracePath,
    [string] $OutputRoot,
    [string] $RunId,
    [string] $ContentTracePath,
    [string] $BrowserExportPath,

    [string] $ApplicationPath,
    [string[]] $ApplicationArguments = @(),
    [string] $ApplicationWorkingDirectory,

    [string] $RouterSshTarget,
    [string[]] $RouterInterfaces = @(),

    [switch] $UseFoundation,
    [string] $FoundationStageRoot,

    [switch] $SkipPacketCapture,
    [switch] $SkipNetworkTrace,
    [switch] $NoPrompt,
    [switch] $SkipAnalysis,
    [switch] $AllowRejectedRun,
    [switch] $DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$scriptRoot = $PSScriptRoot
$repoRoot = (Resolve-Path (Join-Path $scriptRoot '..\..')).Path
. (Join-Path $scriptRoot 'lib\Benchmark.Common.ps1')

$analyzerScript = Join-Path $scriptRoot 'analyze-benchmark.mjs'
$nodeCommand = $null
if (-not $SkipAnalysis -and -not $DryRun) {
    $nodeCommand = Get-Command node -ErrorAction Stop
    if (-not (Test-Path -LiteralPath $analyzerScript -PathType Leaf)) {
        throw "Benchmark analyzer not found: $analyzerScript"
    }
}

if (-not $ProfilePath) {
    $ProfilePath = Join-Path $scriptRoot 'profiles\1080p60-h264.json'
}
if (-not $TracePath) {
    $TracePath = Join-Path $scriptRoot 'traces\20-8-15mbps.json'
}
if (-not $OutputRoot) {
    $OutputRoot = Join-Path $repoRoot 'benchmark-results'
}
if (-not $ApplicationWorkingDirectory) {
    $ApplicationWorkingDirectory = $repoRoot
}

$profile = Get-Content -Raw -LiteralPath $ProfilePath | ConvertFrom-Json
$trace = Get-Content -Raw -LiteralPath $TracePath | ConvertFrom-Json
$measurementSeconds = [double]$profile.timing.measurementSeconds
$traceSeconds = Get-BenchmarkTraceDurationSeconds -Trace $trace
if (-not $SkipNetworkTrace -and [Math]::Abs($traceSeconds - $measurementSeconds) -gt 0.001) {
    throw "Trace duration ($traceSeconds s) must equal profile measurementSeconds ($measurementSeconds s)."
}

if (-not $RunId) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $safeCandidate = $Candidate.ToLowerInvariant()
    $RunId = "$stamp-$safeCandidate-$($profile.name)-$($trace.name)"
}
$runRoot = Join-Path $OutputRoot $RunId
$logsRoot = Join-Path $runRoot 'logs'
$captureRoot = Join-Path $runRoot 'capture'
New-Item -ItemType Directory -Path $logsRoot, $captureRoot -Force | Out-Null

Copy-Item -LiteralPath $ProfilePath -Destination (Join-Path $runRoot 'profile.json') -Force
Copy-Item -LiteralPath $TracePath -Destination (Join-Path $runRoot 'network-trace.json') -Force

$contentTrace = [ordered]@{
    path = if ($ContentTracePath) { (Resolve-Path $ContentTracePath).Path } else { $null }
    sha256 = if ($ContentTracePath) { (Get-FileHash -LiteralPath $ContentTracePath -Algorithm SHA256).Hash.ToLowerInvariant() } else { $null }
    lengthBytes = if ($ContentTracePath) { (Get-Item -LiteralPath $ContentTracePath).Length } else { $null }
}

$manifestPath = Join-Path $runRoot 'manifest.json'
$manifest = [ordered]@{
    schemaVersion = 1
    runId = $RunId
    candidate = $Candidate
    state = 'preparing'
    createdAt = (Get-Date).ToUniversalTime().ToString('o')
    startedAt = $null
    endedAt = $null
    repository = Get-BenchmarkGitSnapshot -Repository $repoRoot
    profile = [ordered]@{ name = $profile.name; path = 'profile.json' }
    networkTrace = [ordered]@{
        name = $trace.name
        path = 'network-trace.json'
        applied = -not $SkipNetworkTrace
        routerSshTarget = if ($RouterSshTarget) { $RouterSshTarget } else { $null }
        routerInterfaces = $RouterInterfaces
    }
    contentTrace = $contentTrace
    application = [ordered]@{
        path = if ($ApplicationPath) { $ApplicationPath } else { $null }
        arguments = $ApplicationArguments
        workingDirectory = $ApplicationWorkingDirectory
        processId = $null
        sha256 = if ($ApplicationPath -and (Test-Path -LiteralPath $ApplicationPath -PathType Leaf)) {
            (Get-FileHash -LiteralPath $ApplicationPath -Algorithm SHA256).Hash.ToLowerInvariant()
        } else { $null }
    }
    foundation = [ordered]@{
        enabled = [bool]$UseFoundation
        stageRoot = if ($FoundationStageRoot) { $FoundationStageRoot } else { $null }
        startResult = $null
        cleanupResult = $null
    }
    packetCapture = [ordered]@{
        enabled = -not $SkipPacketCapture
        format = 'pktmon-etl+pcapng'
        etl = if (-not $SkipPacketCapture) { 'capture/wire.etl' } else { $null }
        pcapng = if (-not $SkipPacketCapture) { 'capture/wire.pcapng' } else { $null }
    }
    browserExport = [ordered]@{
        requestedPath = if ($BrowserExportPath) { $BrowserExportPath } else { $null }
        artifact = $null
        status = if ($BrowserExportPath) { 'pending' } else { 'not-requested' }
    }
    analysis = [ordered]@{
        enabled = -not [bool]$SkipAnalysis
        artifact = if (-not $SkipAnalysis) { 'analysis.json' } else { $null }
        status = if ($SkipAnalysis -or $DryRun) { 'skipped' } else { 'pending' }
        verdict = $null
        accepted = $null
        exitCode = $null
    }
    artifacts = [ordered]@{
        system = 'system.json'
        networkBefore = 'network-before.json'
        networkAfter = 'network-after.json'
        networkDelta = 'network-delta.json'
        traceEvents = 'network-trace-events.json'
    }
    error = $null
}
Write-BenchmarkJson -Value $manifest -Path $manifestPath
Write-BenchmarkJson -Value (Get-BenchmarkSystemSnapshot -Repository $repoRoot) -Path (Join-Path $runRoot 'system.json')

if (-not $SkipPacketCapture -and -not $DryRun) {
    if (-not (Test-BenchmarkAdministrator)) {
        throw 'pktmon capture requires an elevated PowerShell session. Re-run elevated or pass -SkipPacketCapture.'
    }
    if (-not (Get-Command pktmon.exe -ErrorAction SilentlyContinue)) {
        throw 'pktmon.exe is unavailable on this Windows installation.'
    }
}

if (-not $SkipNetworkTrace -and (-not $RouterSshTarget -or $RouterInterfaces.Count -eq 0)) {
    throw 'Network shaping requires -RouterSshTarget and at least one -RouterInterfaces value, or pass -SkipNetworkTrace.'
}
if ($UseFoundation) {
    if (-not $FoundationStageRoot) {
        throw '-UseFoundation requires -FoundationStageRoot.'
    }
    if (-not $DryRun -and -not (Test-BenchmarkAdministrator)) {
        throw 'Foundation swap/restore requires an elevated PowerShell session.'
    }
}

$networkBefore = Get-BenchmarkNetworkSnapshot
Write-BenchmarkJson -Value $networkBefore -Path (Join-Path $runRoot 'network-before.json')

$applicationProcess = $null
$foundationStarted = $false
$captureStarted = $false
$routerCleanupDone = $false
$traceEvents = [System.Collections.Generic.List[object]]::new()
$runError = $null
$etlPath = Join-Path $captureRoot 'wire.etl'
$pcapPath = Join-Path $captureRoot 'wire.pcapng'
$foundationStartResult = Join-Path $logsRoot 'foundation-start.json'
$foundationCleanupResult = Join-Path $logsRoot 'foundation-cleanup.json'
$foundationSentinel = Join-Path $logsRoot 'foundation-complete.sentinel'

function Invoke-RouterPhase {
    param([Parameter(Mandatory = $true)] $Phase)

    foreach ($interface in $RouterInterfaces) {
        $command = "sudo tc qdisc replace dev '$interface' root netem delay $($Phase.delayMs)ms $($Phase.jitterMs)ms distribution normal loss $($Phase.lossPercent)% rate $($Phase.bandwidthMbps)mbit"
        $event = [ordered]@{
            timestamp = (Get-Date).ToUniversalTime().ToString('o')
            action = 'apply'
            phase = $Phase.label
            interface = $interface
            command = $command
            bandwidthMbps = [double]$Phase.bandwidthMbps
            delayMs = [double]$Phase.delayMs
            jitterMs = [double]$Phase.jitterMs
            lossPercent = [double]$Phase.lossPercent
            durationSeconds = [double]$Phase.durationSeconds
        }
        $traceEvents.Add($event)
        Invoke-BenchmarkExternal -FilePath 'ssh' -ArgumentList @($RouterSshTarget, $command) -DryRun:$DryRun
    }
}

function Clear-RouterTrace {
    if ($script:routerCleanupDone -or $SkipNetworkTrace -or -not $RouterSshTarget) {
        return
    }
    foreach ($interface in $RouterInterfaces) {
        $command = "sudo tc qdisc del dev '$interface' root"
        $traceEvents.Add([ordered]@{
            timestamp = (Get-Date).ToUniversalTime().ToString('o')
            action = 'clear'
            interface = $interface
            command = $command
        })
        if ($DryRun) {
            Invoke-BenchmarkExternal -FilePath 'ssh' -ArgumentList @($RouterSshTarget, $command) -DryRun
        } else {
            & ssh $RouterSshTarget $command
            # qdisc may already be absent; cleanup is best-effort and must not hide run results.
        }
    }
    $script:routerCleanupDone = $true
}

function Stop-WireCapture {
    if (-not $script:captureStarted) {
        return
    }
    & pktmon.exe stop
    if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $etlPath)) {
        & pktmon.exe etl2pcap $etlPath --out $pcapPath
    }
    $script:captureStarted = $false
}

try {
    if ($UseFoundation) {
        $hostRoot = Join-Path $scriptRoot 'host'
        $swapScript = Join-Path $hostRoot 'Swap-ToFoundation.ps1'
        $launcherScript = Join-Path $hostRoot 'Start-FoundationPaired.ps1'
        $restoreScript = Join-Path $hostRoot 'Restore-StockSunshine.ps1'
        if ($DryRun) {
            Write-Host "[dry-run] Foundation swap from $FoundationStageRoot"
        } else {
            & $swapScript `
                -StageRoot $FoundationStageRoot `
                -LauncherScript $launcherScript `
                -RestoreScript $restoreScript `
                -ResultPath $foundationStartResult `
                -CompletionSentinel $foundationSentinel `
                -Port 47989
            $foundationStarted = $true
            $manifest.foundation.startResult = 'logs/foundation-start.json'
        }
    }

    if ($ApplicationPath) {
        if (-not (Test-Path -LiteralPath $ApplicationPath -PathType Leaf)) {
            throw "Application executable not found: $ApplicationPath"
        }
        $stdoutPath = Join-Path $logsRoot 'application.stdout.log'
        $stderrPath = Join-Path $logsRoot 'application.stderr.log'
        if ($DryRun) {
            Write-Host "[dry-run] start $ApplicationPath $($ApplicationArguments -join ' ')"
        } else {
            $applicationProcess = Start-Process `
                -FilePath $ApplicationPath `
                -ArgumentList $ApplicationArguments `
                -WorkingDirectory $ApplicationWorkingDirectory `
                -RedirectStandardOutput $stdoutPath `
                -RedirectStandardError $stderrPath `
                -PassThru
            $manifest.application.processId = $applicationProcess.Id
        }
    }

    if (-not $NoPrompt -and -not $DryRun) {
        Write-Host ''
        Write-Host "Run directory: $runRoot"
        Write-Host 'Open the candidate, start the deterministic content trace, enable BetterParsec Stats when applicable, then press Enter.'
        [void](Read-Host)
    }

    if (-not $SkipPacketCapture) {
        if ($DryRun) {
            Write-Host "[dry-run] pktmon start -> $etlPath"
        } else {
            & pktmon.exe start --capture --pkt-size 0 --file-name $etlPath
            if ($LASTEXITCODE -ne 0) {
                throw "pktmon start failed with exit code $LASTEXITCODE"
            }
            $captureStarted = $true
        }
    }

    $manifest.state = 'running'
    $manifest.startedAt = (Get-Date).ToUniversalTime().ToString('o')
    Write-BenchmarkJson -Value $manifest -Path $manifestPath

    $warmupSeconds = [double]$profile.timing.warmupSeconds
    if ($DryRun) {
        Write-Host "[dry-run] warm-up $warmupSeconds seconds"
    } elseif ($warmupSeconds -gt 0) {
        Start-Sleep -Seconds $warmupSeconds
    }

    if ($SkipNetworkTrace) {
        if ($DryRun) {
            Write-Host "[dry-run] unshaped measurement $measurementSeconds seconds"
        } else {
            Start-Sleep -Seconds $measurementSeconds
        }
    } else {
        foreach ($phase in $trace.phases) {
            Invoke-RouterPhase -Phase $phase
            if ($DryRun) {
                Write-Host "[dry-run] hold phase '$($phase.label)' for $($phase.durationSeconds) seconds"
            } else {
                Start-Sleep -Seconds ([double]$phase.durationSeconds)
            }
        }
    }

    # End the measured interval before any export UI or cleanup traffic.
    Clear-RouterTrace
    Stop-WireCapture

    if ($BrowserExportPath -and -not $NoPrompt -and -not $DryRun) {
        Write-Host "Export BetterParsec Benchmark JSON to: $BrowserExportPath"
        Write-Host 'Press Enter after the file exists.'
        [void](Read-Host)
    }

    $manifest.state = 'completed'
}
catch {
    $runError = $_
    $manifest.state = 'failed'
    $manifest.error = $_.Exception.Message
}
finally {
    Clear-RouterTrace

    Stop-WireCapture

    if ($applicationProcess -and -not $applicationProcess.HasExited) {
        $actual = Get-CimInstance Win32_Process -Filter "ProcessId=$($applicationProcess.Id)" -ErrorAction SilentlyContinue
        if ($actual -and [IO.Path]::GetFullPath($actual.ExecutablePath) -eq [IO.Path]::GetFullPath($ApplicationPath)) {
            Stop-Process -Id $applicationProcess.Id -Force
        }
    }

    if ($foundationStarted) {
        try {
            & (Join-Path $scriptRoot 'host\Finish-FoundationTest.ps1') `
                -StageRoot $FoundationStageRoot `
                -TestResultPath $foundationStartResult `
                -CleanupResultPath $foundationCleanupResult
            $manifest.foundation.cleanupResult = 'logs/foundation-cleanup.json'

            $foundationState = Get-Content -Raw -LiteralPath $foundationStartResult | ConvertFrom-Json
            foreach ($source in @($foundationState.SunshineLog, $foundationState.StdoutLog, $foundationState.StderrLog)) {
                if ($source -and (Test-Path -LiteralPath $source -PathType Leaf)) {
                    Copy-Item -LiteralPath $source -Destination $logsRoot -Force
                }
            }
        }
        catch {
            if (-not $manifest.error) {
                $manifest.error = "Foundation cleanup failed: $($_.Exception.Message)"
            }
            $manifest.state = 'failed'
        }
    }

    $networkAfter = Get-BenchmarkNetworkSnapshot
    Write-BenchmarkJson -Value $networkAfter -Path (Join-Path $runRoot 'network-after.json')
    Write-BenchmarkJson -Value (Get-BenchmarkNetworkDelta -Before $networkBefore -After $networkAfter) -Path (Join-Path $runRoot 'network-delta.json')
    Write-BenchmarkJson -Value $traceEvents -Path (Join-Path $runRoot 'network-trace-events.json')

    if ($BrowserExportPath) {
        if (Test-Path -LiteralPath $BrowserExportPath -PathType Leaf) {
            $destination = Join-Path $runRoot 'browser-benchmark.json'
            Copy-Item -LiteralPath $BrowserExportPath -Destination $destination -Force
            $manifest.browserExport.artifact = 'browser-benchmark.json'
            $manifest.browserExport.status = 'collected'
        } else {
            $manifest.browserExport.status = 'missing'
        }
    }

    if (Test-Path -LiteralPath $pcapPath -PathType Leaf) {
        $manifest.packetCapture.pcapngSha256 = (Get-FileHash -LiteralPath $pcapPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $manifest.packetCapture.pcapngLengthBytes = (Get-Item -LiteralPath $pcapPath).Length
    }
    if (Test-Path -LiteralPath $etlPath -PathType Leaf) {
        $manifest.packetCapture.etlSha256 = (Get-FileHash -LiteralPath $etlPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $manifest.packetCapture.etlLengthBytes = (Get-Item -LiteralPath $etlPath).Length
    }

    $manifest.endedAt = (Get-Date).ToUniversalTime().ToString('o')
    Write-BenchmarkJson -Value $manifest -Path $manifestPath
}

$analysisRejected = $false
$analysisExecutionError = $null
if (-not $SkipAnalysis -and -not $DryRun) {
    $analysisPath = Join-Path $runRoot 'analysis.json'
    $analysisProcess = Start-Process `
        -FilePath $nodeCommand.Source `
        -ArgumentList @(
            $analyzerScript,
            '--run', $runRoot,
            '--output', $analysisPath
        ) `
        -NoNewWindow `
        -Wait `
        -PassThru

    $manifest.analysis.exitCode = $analysisProcess.ExitCode
    if (Test-Path -LiteralPath $analysisPath -PathType Leaf) {
        $analysis = Get-Content -Raw -LiteralPath $analysisPath | ConvertFrom-Json
        $manifest.analysis.status = 'collected'
        $manifest.analysis.verdict = $analysis.verdict
        $manifest.analysis.accepted = [bool]$analysis.accepted
        $analysisRejected = -not [bool]$analysis.accepted
    } else {
        $manifest.analysis.status = 'failed'
    }

    if ($analysisProcess.ExitCode -eq 1) {
        $analysisExecutionError = 'Benchmark analyzer failed to produce a valid verdict.'
    } elseif ($analysisProcess.ExitCode -notin @(0, 2)) {
        $analysisExecutionError = "Benchmark analyzer returned unexpected exit code $($analysisProcess.ExitCode)."
    }

    Write-BenchmarkJson -Value $manifest -Path $manifestPath
}

if ($runError) {
    throw $runError
}
if ($analysisExecutionError) {
    throw $analysisExecutionError
}
if ($analysisRejected -and -not $AllowRejectedRun) {
    throw "Benchmark truth gate rejected run '$RunId'. Review $runRoot\analysis.json."
}

Write-Host "Benchmark run complete: $runRoot"
