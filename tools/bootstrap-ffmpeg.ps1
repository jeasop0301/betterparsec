# Pinned FFmpeg (LGPL shared) for app-native video decode (A0 slice 2).
# Reproducible source of truth: URL + SHA-256 pin; extract lands in
# third-party/ffmpeg (gitignored). Dynamic linking keeps us LGPL-clean
# (unified-app-architecture.md 4-1).
$ErrorActionPreference = 'Stop'

$url = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n7.1-latest-win64-lgpl-shared-7.1.zip'
$sha256 = '6949f6236f374ad6d1c9ccad47d3780bce025edff49b04b549923046f691a1af'

$root = Split-Path -Parent $PSScriptRoot
$thirdParty = Join-Path $root 'third-party'
$zip = Join-Path $thirdParty 'ffmpeg-lgpl-shared.zip'
$dest = Join-Path $thirdParty 'ffmpeg'

if (Test-Path (Join-Path $dest 'lib\avcodec.lib')) {
    $existing = (Get-FileHash $zip -Algorithm SHA256 -ErrorAction SilentlyContinue).Hash
    if ($existing -and $existing.ToLower() -eq $sha256) {
        Write-Host 'ffmpeg already bootstrapped (pin matches)'
        exit 0
    }
}

New-Item -ItemType Directory -Path $thirdParty -Force | Out-Null
if (-not (Test-Path $zip) -or ((Get-FileHash $zip -Algorithm SHA256).Hash.ToLower() -ne $sha256)) {
    Write-Host "downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $zip
}
$got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
if ($got -ne $sha256) { throw "SHA-256 mismatch: got $got want $sha256" }

Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
Expand-Archive -Force $zip -DestinationPath $thirdParty
Move-Item (Join-Path $thirdParty 'ffmpeg-n7.1-latest-win64-lgpl-shared-7.1') $dest
Write-Host "ffmpeg bootstrapped at $dest"
