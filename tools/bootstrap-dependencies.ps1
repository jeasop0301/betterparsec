[CmdletBinding()]
param(
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$vendorRoot = Join-Path $repoRoot 'vendor'
$target = Join-Path $vendorRoot 'moonlight-common-rust'
$commonC = Join-Path $target 'moonlight-common-sys\moonlight-common-c'
$rustPatch = Join-Path $repoRoot 'patches\moonlight-common-rust.patch'
$commonCPatch = Join-Path $repoRoot 'patches\moonlight-common-c.patch'

$rustUrl = 'https://github.com/MrCreativ3001/moonlight-common-rust.git'
$rustRevision = 'df9f1e3003fb4834dbb17a4bd4d3cf25d2fea3d9'
$commonCRevision = '62687809b1f7410c3db4be2527503a54ae408d70'

function Invoke-Git {
    param([Parameter(Mandatory = $true)][string[]] $Arguments)

    & git @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "git failed with exit code ${LASTEXITCODE}: git $($Arguments -join ' ')"
    }
}

function Get-GitHead {
    param([Parameter(Mandatory = $true)][string] $Repository)

    $head = (& git -C $Repository rev-parse HEAD 2>$null)
    if ($LASTEXITCODE -ne 0) {
        return $null
    }
    return $head.Trim()
}

function Test-PatchApplied {
    param(
        [Parameter(Mandatory = $true)][string] $Repository,
        [Parameter(Mandatory = $true)][string] $Patch
    )

    & git -C $Repository apply --reverse --check --ignore-space-change --ignore-whitespace $Patch *> $null
    return $LASTEXITCODE -eq 0
}

function Apply-BetterParsecPatches {
    Invoke-Git @('-C', $target, 'apply', '--check', '--ignore-space-change', '--ignore-whitespace', $rustPatch)
    Invoke-Git @('-C', $target, 'apply', '--ignore-space-change', '--ignore-whitespace', '--whitespace=nowarn', $rustPatch)
    Invoke-Git @('-C', $commonC, 'apply', '--check', '--ignore-space-change', '--ignore-whitespace', $commonCPatch)
    Invoke-Git @('-C', $commonC, 'apply', '--ignore-space-change', '--ignore-whitespace', '--whitespace=nowarn', $commonCPatch)
}

function Test-CleanPinnedBase {
    if ((Get-GitHead $target) -ne $rustRevision -or (Get-GitHead $commonC) -ne $commonCRevision) {
        return $false
    }
    & git -C $target diff --quiet --ignore-submodules=dirty
    $rustClean = $LASTEXITCODE -eq 0
    & git -C $commonC diff --quiet
    $commonCClean = $LASTEXITCODE -eq 0
    return $rustClean -and $commonCClean
}

function Test-Ready {
    if (-not (Test-Path -LiteralPath (Join-Path $target '.git'))) {
        return $false
    }
    if ((Get-GitHead $target) -ne $rustRevision) {
        return $false
    }
    if (-not (Test-Path -LiteralPath (Join-Path $commonC '.git'))) {
        return $false
    }
    if ((Get-GitHead $commonC) -ne $commonCRevision) {
        return $false
    }
    return (Test-PatchApplied -Repository $target -Patch $rustPatch) -and
        (Test-PatchApplied -Repository $commonC -Patch $commonCPatch)
}

if (Test-Ready) {
    Write-Host "Pinned BetterParsec dependency is ready: $target"
    exit 0
}

if (Test-Path -LiteralPath $target) {
    if (Test-CleanPinnedBase) {
        Apply-BetterParsecPatches
        if (-not (Test-Ready)) {
            throw 'Existing pinned dependency was patched, but verification failed.'
        }
        Write-Host "Completed patches in existing pinned dependency: $target"
        exit 0
    }
    if (-not $Force) {
        throw "Dependency directory exists but is incomplete or differs from the pinned revisions: $target. Re-run with -Force to recreate only this generated vendor directory."
    }
    Remove-Item -LiteralPath $target -Recurse -Force
}

New-Item -ItemType Directory -Path $vendorRoot -Force | Out-Null
Invoke-Git @('clone', '--no-checkout', $rustUrl, $target)
Invoke-Git @('-C', $target, 'config', 'core.autocrlf', 'false')
Invoke-Git @('-C', $target, 'checkout', '--detach', $rustRevision)
Invoke-Git @('-C', $target, 'submodule', 'update', '--init', '--recursive')

# Normalize the freshly generated submodule before applying a line-oriented patch.
Invoke-Git @('-C', $commonC, 'config', 'core.autocrlf', 'false')
Invoke-Git @('-C', $commonC, 'reset', '--hard', $commonCRevision)

Apply-BetterParsecPatches

if (-not (Test-Ready)) {
    throw 'Dependency bootstrap completed, but revision or patch verification failed.'
}

Write-Host "Bootstrapped pinned BetterParsec dependency: $target"
Write-Host "  moonlight-common-rust: $rustRevision + patches/moonlight-common-rust.patch"
Write-Host "  moonlight-common-c:    $commonCRevision + patches/moonlight-common-c.patch"
