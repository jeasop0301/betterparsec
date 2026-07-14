# Pinned libclang for bindgen (ffmpeg-sys-next, app-native `video` feature).
# The machine-wide LLVM can be too new for the pinned bindgen (LLVM 22
# makes bindgen 0.70 emit opaque structs), so we pin libclang 18.1.1 from
# the LLVM-owned PyPI `libclang` wheel (just libclang.dll, ~26 MB).
# Extract lands in third-party/libclang (gitignored); .cargo/config.toml
# points LIBCLANG_PATH there.
$ErrorActionPreference = 'Stop'

$url = 'https://files.pythonhosted.org/packages/0b/2d/3f480b1e1d31eb3d6de5e3ef641954e5c67430d5ac93b7fa7e07589576c7/libclang-18.1.1-py2.py3-none-win_amd64.whl'
$sha256 = '4dd2d3b82fab35e2bf9ca717d7b63ac990a3519c7e312f19fa8e86dcc712f7fb'

$root = Split-Path -Parent $PSScriptRoot
$thirdParty = Join-Path $root 'third-party'
$wheel = Join-Path $thirdParty 'libclang-18.1.1-win_amd64.whl'
$dest = Join-Path $thirdParty 'libclang'

if (Test-Path (Join-Path $dest 'libclang.dll')) {
    $existing = (Get-FileHash $wheel -Algorithm SHA256 -ErrorAction SilentlyContinue).Hash
    if ($existing -and $existing.ToLower() -eq $sha256) {
        Write-Host 'libclang already bootstrapped (pin matches)'
        exit 0
    }
}

New-Item -ItemType Directory -Path $thirdParty -Force | Out-Null
if (-not (Test-Path $wheel) -or ((Get-FileHash $wheel -Algorithm SHA256).Hash.ToLower() -ne $sha256)) {
    Write-Host "downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $wheel
}
$got = (Get-FileHash $wheel -Algorithm SHA256).Hash.ToLower()
if ($got -ne $sha256) { throw "SHA-256 mismatch: got $got want $sha256" }

# A wheel is a zip; this one carries the DLL at
# libclang-18.1.1.data/platlib/clang/native/libclang.dll — locate it by
# name to stay robust against layout shuffles between wheel revisions.
$staging = Join-Path $thirdParty 'libclang-wheel-staging'
Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::ExtractToDirectory($wheel, $staging)
$dll = Get-ChildItem -Recurse $staging -Filter 'libclang.dll' | Select-Object -First 1
if (-not $dll) { throw 'libclang.dll not found inside the wheel' }
Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $dest -Force | Out-Null
Move-Item $dll.FullName (Join-Path $dest 'libclang.dll')
Remove-Item $staging -Recurse -Force
Write-Host "libclang bootstrapped at $dest"
