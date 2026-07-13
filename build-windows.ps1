# Windows build: prefer an installed MSVC OpenSSL SDK; otherwise build the
# vendored OpenSSL source with a complete Strawberry Perl + NASM toolchain.
# Usage: .\build-windows.ps1 [--release]

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

& (Join-Path $PSScriptRoot "tools\bootstrap-dependencies.ps1")

if (-not $env:OPENSSL_DIR) {
    $candidates = @(
        "C:\Program Files\OpenSSL-Win64",
        "C:\Program Files\OpenSSL",
        "C:\OpenSSL-Win64",
        "C:\OpenSSL"
    )
    foreach ($dir in $candidates) {
        $h = Join-Path $dir "include\openssl\ssl.h"
        if (Test-Path $h) {
            $env:OPENSSL_DIR = $dir
            Write-Host "Using OPENSSL_DIR=$dir"
            break
        }
    }
    if (-not $env:OPENSSL_DIR) {
        $strawberryPerl = "C:\Strawberry\perl\bin\perl.exe"
        $strawberryPerlDir = Split-Path -Parent $strawberryPerl
        $strawberryNasm = "C:\Strawberry\c\bin\nasm.exe"
        $strawberryNasmDir = Split-Path -Parent $strawberryNasm
        $strawberryReady = (Test-Path -LiteralPath $strawberryPerl -PathType Leaf) -and
            (Test-Path -LiteralPath $strawberryNasm -PathType Leaf)

        if ($strawberryReady) {
            & $strawberryPerl -MLocale::Maketext::Simple -e "1"
            $strawberryReady = $LASTEXITCODE -eq 0
        }

        if ($strawberryReady) {
            # Do not use C:\Strawberry\c as OPENSSL_DIR: its libraries target
            # MinGW, while the default Rust Windows toolchain is MSVC. We only
            # use Strawberry's complete Perl/NASM tools to build openssl-src.
            $env:PATH = "$strawberryPerlDir;$strawberryNasmDir;$env:PATH"
            Write-Host "No MSVC OpenSSL SDK found; using vendored OpenSSL with Strawberry Perl and NASM."
        } else {
            Write-Host "MSVC OpenSSL SDK not found in: $($candidates -join ', ')"
            Write-Host ""
            Write-Host "Option 1 - Install Win64 OpenSSL (default path):"
            Write-Host "  https://slproweb.com/products/Win32OpenSSL.html"
            Write-Host "  Download 'Win64 OpenSSL v3.x' and install to a default path."
            Write-Host ""
            Write-Host "Option 2 - Install Strawberry Perl with NASM so openssl-src can build vendored OpenSSL."
            Write-Host ""
            Write-Host "Option 3 - Set an existing MSVC-compatible SDK explicitly:"
            Write-Host "  `$env:OPENSSL_DIR = 'C:\path\to\your\OpenSSL-Win64'"
            exit 1
        }
    }
}

$release = ($args -contains '--release')
if ($release) {
    & cargo build --release
} else {
    & cargo build
}
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}
