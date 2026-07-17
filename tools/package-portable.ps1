# Package the native client as a portable zip (exe + FFmpeg DLLs + run
# script). The staging server URL/ids are baked into run-incheon.bat for
# the current WAN test setup; edit the bat for other deployments.
#
# -HostBundle (G006): additionally stages a complete host set
# (betterparsec.exe + streamer.exe + a Foundation payload placeholder +
# runtime assets) under server/betterparsec-host-bundle/ and signs a
# complete-set manifest for it via `cargo run -p betterparsec-updater --
# sign` (dev key only -- see tools/updater/src/lib.rs's
# DEV_SIGNING_KEY_SEED doc; release signing happens outside this repo,
# on a machine holding the real signing key, never in CI/this script).
# This is build/staging only: nothing under -HostBundle is ever
# uploaded, published, or deployed by this script (that stays
# tools/publish-cf.ps1's job for the existing client-only zip, and
# publishing a host bundle anywhere is out of scope until G007 P0 live
# closure).
param(
    [switch]$HostBundle
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$release = Join-Path $root 'target\release'
$staging = Join-Path $root 'server\betterparsec-portable'
$zip = Join-Path $root 'server\betterparsec-portable.zip'

Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $staging | Out-Null

$files = @(
    'betterparsec.exe',
    # Field diagnosis for the 07-17 Alt+Tab issue: run hook-probe.exe on
    # the client machine — exit 0 / "events observed: ~10" proves LL
    # keyboard hooks work there; 0 events means something on that machine
    # blocks them (security software / conflicting hook).
    'hook-probe.exe',
    'avcodec-61.dll', 'avdevice-61.dll', 'avfilter-10.dll',
    'avformat-61.dll', 'avutil-59.dll', 'swresample-5.dll', 'swscale-8.dll'
)
foreach ($f in $files) {
    Copy-Item (Join-Path $release $f) $staging
}

@(
    '@echo off',
    'set BP_URL=https://121.167.176.91:8080',
    'set BP_USER=kje12e4',
    'set BP_HOST_ID=2062835576',
    'set BP_APP_ID=881448767',
    'set RUST_LOG=info,betterparsec::input=debug',
    'start betterparsec.exe'
) | Set-Content -Path (Join-Path $staging 'run-incheon.bat')

# Zero-config defaults: the exe reads this next to itself and pre-fills the
# connect form (Parsec-style — download, run, type only the password). Keep
# the values in sync with run-incheon.bat above; the password is never baked.
@(
    'base_url=https://121.167.176.91:8080',
    'username=kje12e4',
    'host_id=2062835576',
    'app_id=881448767'
) | Set-Content -Path (Join-Path $staging 'betterparsec.conf')

Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -Force
# Self-serve: the release web-server serves static/ at the site root, so
# the zip is downloadable at https://<server>:8080/betterparsec-portable.zip
# (re-run this script after every `Copy-Item dist static` redeploy — the
# redeploy wipes static/).
# NOTE: the Cloudflare distribution site (betterparsec.kje12e4.workers.dev)
# streams the zip from R2, NOT from static/ — updating that path requires
# tools/publish-cf.ps1 (which re-runs this script, uploads to R2, deploys).
# This script alone only refreshes the self-served :8080 copy.
$staticDir = Join-Path $root 'static'
if (Test-Path $staticDir) {
    Copy-Item $zip $staticDir -Force
}
Write-Output ("zip: {0} ({1:N1} MB)" -f $zip, ((Get-Item $zip).Length / 1MB))


if ($HostBundle) {
    # G006 host complete-set staging. betterparsec.exe is copied straight
    # from $staging above so its bytes (and therefore its sha256 in the
    # manifest) are byte-identical to the client zip's copy -- one build
    # of the exe serves both roles, which is exactly the invariant
    # update.rs's `check_internally_consistent` enforces at verify time.
    $hostStage = Join-Path $root 'server\betterparsec-host-bundle'
    Remove-Item -Recurse -Force $hostStage -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $hostStage | Out-Null

    Copy-Item (Join-Path $staging 'betterparsec.exe') $hostStage
    Copy-Item (Join-Path $release 'streamer.exe') $hostStage

    # Foundation (Sunshine) is a managed subprocess whose real binary is
    # never redistributed from this repo (bootstrap-dependencies fetches
    # it separately per docs/DEPENDENCIES.md); this placeholder just
    # reserves the manifest slot and byte-for-byte identifies "no real
    # payload staged" so a later real packaging step has something
    # concrete to overwrite before a set is ever signed for real use.
    Set-Content -Path (Join-Path $hostStage 'foundation-payload.placeholder') `
        -Value 'G006 placeholder: replace with the staged Foundation (Sunshine) payload before shipping this set.'

    # Runtime assets shared by the host role's embedded web-server
    # (static/ landing + client zip it can self-serve).
    New-Item -ItemType Directory -Path (Join-Path $hostStage 'assets') -Force | Out-Null
    Copy-Item $zip (Join-Path $hostStage 'assets\betterparsec-portable.zip') -Force

    # Written *outside* $hostStage (a sibling file, not inside the
    # staged directory) so `verify_staged_set`'s unlisted-file check
    # only ever needs to allow-list `manifest.json` itself -- the spec
    # file was never part of the signed/installed set to begin with.
    $spec = Join-Path $root 'server\betterparsec-host-bundle.component-spec.json'
    @'
[
  { "name": "betterparsec.exe", "role": "shared" },
  { "name": "streamer.exe", "role": "host" },
  { "name": "foundation-payload.placeholder", "role": "host" },
  { "name": "assets/betterparsec-portable.zip", "role": "shared" }
]
'@ | Set-Content -Path $spec

    $setVersion = Get-Date -Format 'yyyy.M.d'
    # Manifest lives INSIDE the staged directory (not beside it) --
    # this is the one pinned layout `app-native/src/update.rs`'s
    # `staged_dir`/`manifest_path` helpers and the host status panel
    # both assume; see that module's "Staged-set layout" doc.
    $manifest = Join-Path $hostStage 'manifest.json'
    Push-Location $root
    try {
        # Dev key only (tools/updater/src/lib.rs::DEV_SIGNING_KEY_SEED) --
        # release signing happens outside this repo/script, on a machine
        # holding the real signing key.
        cargo run -p betterparsec-updater -- sign `
            --staged $hostStage `
            --spec $spec `
            --set-version $setVersion `
            --signing-key-id dev-trust-root-v1 `
            --out $manifest
        if ($LASTEXITCODE -ne 0) {
            throw "betterparsec-updater sign failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }

    Write-Output ("host bundle staged (dev-signed, NOT published): {0}" -f $hostStage)
}
