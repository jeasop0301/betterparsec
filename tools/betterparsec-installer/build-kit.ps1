# Assemble the G007 keyboard-capture test kit on the BUILD machine (where
# PowerShell and the WDK/signtool are available). The output folder is copied
# whole to the locked test client, where the tester just double-clicks
# betterparsec-installer.exe (UAC elevates it; no PowerShell needed there).
#
# Output (server\g007-kit\):
#   betterparsec-installer.exe   native UAC installer (release, requireAdministrator)
#   betterparsec-kbdflt.sys      kernel keyboard filter, test-signed here
#   betterparsec-kbdflt.cer      public test cert the installer trusts on the client
#   input-broker.exe             LocalSystem broker
#   KIT-README.txt               tester instructions
#
# The client .exe (betterparsec.exe) is distributed separately (portable zip /
# Cloudflare); this kit is only the driver+broker capture path that the blocked
# PowerShell scripts could not install.
[CmdletBinding()]
param(
    [string]$OutDir = (Join-Path $PSScriptRoot '..\..\server\g007-kit')
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

# 1. Release binaries (installer + broker).
Write-Host '[1/4] Building release binaries'
& cargo build --release -p input-broker -p betterparsec-installer
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

# 2. Build the kernel driver .sys.
Write-Host '[2/4] Building the keyboard filter driver'
& (Join-Path $root 'drivers\betterparsec-kbdflt\build-test.cmd')
if ($LASTEXITCODE -ne 0) { throw 'driver build failed' }
$sys = Join-Path $root 'drivers\betterparsec-kbdflt\out\x64\betterparsec-kbdflt.sys'
if (-not (Test-Path -LiteralPath $sys)) { throw "driver not found: $sys" }

# 3. Sign the .sys with a per-user test cert and export its public .cer.
Write-Host '[3/4] Signing the driver (test cert)'
$cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq 'CN=BetterParsec Test Driver' -and $_.HasPrivateKey } |
    Sort-Object NotAfter -Descending | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert `
        -Subject 'CN=BetterParsec Test Driver' `
        -CertStoreLocation Cert:\CurrentUser\My `
        -HashAlgorithm SHA256 -NotAfter (Get-Date).AddYears(2)
}
$signtool = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe'
if (-not (Test-Path -LiteralPath $signtool)) { throw "signtool.exe not found: $signtool" }
& $signtool sign /v /fd SHA256 /sha1 $cert.Thumbprint $sys
if ($LASTEXITCODE -ne 0) { throw 'signtool failed' }
# The cert is self-signed, so signtool verify (trusted-root policy) always fails
# on the build machine — the installer trusts the cert on the client. Verify
# instead that the file carries a signature from exactly our cert.
$sig = Get-AuthenticodeSignature -LiteralPath $sys
if (-not $sig.SignerCertificate -or $sig.SignerCertificate.Thumbprint -ne $cert.Thumbprint) {
    throw "driver is not signed with the expected test cert (got: $($sig.Status) / $($sig.SignerCertificate.Thumbprint))"
}

# 4. Assemble the kit folder.
Write-Host '[4/4] Assembling kit'
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
Copy-Item (Join-Path $root 'target\release\betterparsec-installer.exe') $OutDir -Force
Copy-Item (Join-Path $root 'target\release\input-broker.exe') $OutDir -Force
Copy-Item $sys $OutDir -Force
Export-Certificate -Cert $cert -FilePath (Join-Path $OutDir 'betterparsec-kbdflt.cer') -Force | Out-Null
Copy-Item (Join-Path $PSScriptRoot 'KIT-README.txt') $OutDir -Force -ErrorAction SilentlyContinue

Write-Host ''
Write-Host "G007 kit ready: $OutDir"
Write-Host 'Copy that folder to the test client and double-click betterparsec-installer.exe.'
