# Package the native client as a portable zip (exe + FFmpeg DLLs + run
# script). The staging server URL/ids are baked into run-incheon.bat for
# the current WAN test setup; edit the bat for other deployments.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$release = Join-Path $root 'target\release'
$staging = Join-Path $root 'server\betterparsec-portable'
$zip = Join-Path $root 'server\betterparsec-portable.zip'

Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $staging | Out-Null

$files = @(
    'betterparsec.exe',
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

Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -Force
Write-Output ("zip: {0} ({1:N1} MB)" -f $zip, ((Get-Item $zip).Length / 1MB))
