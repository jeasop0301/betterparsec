[CmdletBinding()]
param(
    [string] $CandidateA = 'BetterParsec',
    [string] $CandidateB = 'ParsecNative',
    [string[]] $Cells = @('clean-lan', 'typical-wan', 'bad-wifi', 'congested-wan', 'recovery'),
    [ValidateRange(1, 100)]
    [int] $Repetitions = 5,
    [int] $Seed = [Environment]::TickCount,
    [string] $Profile = 'profiles/1080p60-h264.json',
    [string] $Trace = 'traces/20-8-15mbps.json',
    [string] $OutputPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $OutputPath) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
    $OutputPath = Join-Path $repoRoot "benchmark-results\plans\abba-$stamp.json"
}

$random = [Random]::new($Seed)
$runs = [System.Collections.Generic.List[object]]::new()
$runIndex = 0

foreach ($cell in $Cells) {
    for ($repetition = 1; $repetition -le $Repetitions; $repetition++) {
        $forward = $random.Next(0, 2) -eq 0
        $sequenceName = if ($forward) { 'ABBA' } else { 'BAAB' }
        $sequence = if ($forward) {
            @($CandidateA, $CandidateB, $CandidateB, $CandidateA)
        } else {
            @($CandidateB, $CandidateA, $CandidateA, $CandidateB)
        }

        for ($position = 0; $position -lt $sequence.Count; $position++) {
            $runIndex++
            $runs.Add([ordered]@{
                index = $runIndex
                runId = ('{0:D3}-{1}-r{2:D2}-p{3}-{4}' -f $runIndex, $cell, $repetition, ($position + 1), $sequence[$position].ToLowerInvariant())
                cell = $cell
                repetition = $repetition
                sequence = $sequenceName
                sequencePosition = $position + 1
                candidate = $sequence[$position]
                profile = $Profile
                trace = $Trace
                status = 'pending'
                resultPath = $null
                rejectionReason = $null
            })
        }
    }
}

$plan = [ordered]@{
    schemaVersion = 1
    createdAt = (Get-Date).ToUniversalTime().ToString('o')
    seed = $Seed
    candidateA = $CandidateA
    candidateB = $CandidateB
    cells = $Cells
    repetitionsPerCell = $Repetitions
    runsPerBlock = 4
    totalRuns = $runs.Count
    randomization = 'Each cell/repetition independently selects ABBA or BAAB with the recorded seed.'
    runs = $runs
}

$directory = Split-Path -Parent $OutputPath
if ($directory) {
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
}
$plan | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutputPath -Encoding UTF8
Write-Host "ABBA plan created: $OutputPath"
Write-Host "Total runs: $($runs.Count)"
