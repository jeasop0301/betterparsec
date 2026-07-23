[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$DriverSys = "$PSScriptRoot\out\x64\betterparsec-kbdflt.sys",
    [switch]$EnableTestSigning
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell window.'
}
if (-not (Test-Path -LiteralPath $DriverSys -PathType Leaf)) {
    throw "Driver not found: $DriverSys. Run build-test.cmd first."
}

$signtool = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe'
if (-not (Test-Path -LiteralPath $signtool -PathType Leaf)) {
    throw "signtool.exe not found: $signtool"
}

$certificate = Get-ChildItem Cert:\LocalMachine\My |
    Where-Object { $_.Subject -eq 'CN=BetterParsec Test Driver' -and $_.HasPrivateKey } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1
if (-not $certificate) {
    if (-not $PSCmdlet.ShouldProcess('LocalMachine certificate stores', 'Create and trust BetterParsec test driver certificate')) {
        return
    }
    $certificate = New-SelfSignedCertificate -Type CodeSigningCert `
        -Subject 'CN=BetterParsec Test Driver' `
        -CertStoreLocation Cert:\LocalMachine\My `
        -HashAlgorithm SHA256 `
        -NotAfter (Get-Date).AddYears(2)
    $cer = Join-Path $env:TEMP 'betterparsec-test-driver.cer'
    Export-Certificate -Cert $certificate -FilePath $cer -Force | Out-Null
    Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
    Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
    Remove-Item -LiteralPath $cer -Force
}

if ($PSCmdlet.ShouldProcess($DriverSys, 'Apply embedded test signature')) {
    & $signtool sign /v /fd SHA256 /sha1 $certificate.Thumbprint /sm $DriverSys
    if ($LASTEXITCODE -ne 0) { throw 'signtool failed.' }
    & $signtool verify /v /kp $DriverSys
    if ($LASTEXITCODE -ne 0) { throw 'Kernel-policy signature verification failed.' }
}

if ($EnableTestSigning) {
    if ($PSCmdlet.ShouldProcess('current Windows boot configuration', 'Enable TESTSIGNING')) {
        & bcdedit.exe /set testsigning on
        if ($LASTEXITCODE -ne 0) {
            throw 'BCDEdit failed. Secure Boot may need to be disabled before test-signing mode can be enabled.'
        }
        Write-Host 'Windows test-signing mode enabled. Reboot is required.'
    }
} else {
    Write-Host 'Driver signed. TESTSIGNING was not changed; pass -EnableTestSigning explicitly when using this test certificate.'
}
