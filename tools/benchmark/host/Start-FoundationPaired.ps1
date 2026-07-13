param(
    [Parameter(Mandatory = $true)]
    [string] $StageRoot,
    [Parameter(Mandatory = $true)]
    [string] $ResultPath,
    [int] $Port = 49000
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'This helper must run elevated.'
}

$stockConfig = 'C:\Program Files\Sunshine\config'
$foundationExe = Join-Path $StageRoot 'sunshine.exe'
if (-not (Test-Path -LiteralPath $foundationExe -PathType Leaf)) {
    throw "Foundation executable not found: $foundationExe"
}
if (-not (Test-Path -LiteralPath $stockConfig -PathType Container)) {
    throw "Stock configuration not found: $stockConfig"
}

$occupied = Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue
if ($occupied) {
    throw "TCP port $Port is already in use."
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$liveRoot = Join-Path $StageRoot "paired-live-$stamp"
$liveConfig = Join-Path $liveRoot 'config'
$stdoutPath = Join-Path $liveRoot 'foundation.stdout.log'
$stderrPath = Join-Path $liveRoot 'foundation.stderr.log'
New-Item -ItemType Directory -Path $liveConfig -Force | Out-Null
New-Item -ItemType Directory -Path (Split-Path -Parent $ResultPath) -Force | Out-Null

$directorySecurity = [Security.AccessControl.DirectorySecurity]::new()
$directorySecurity.SetAccessRuleProtection($true, $false)
$inheritance = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$propagation = [Security.AccessControl.PropagationFlags]::None
$allow = [Security.AccessControl.AccessControlType]::Allow
foreach ($sid in @(
    $identity.User,
    [Security.Principal.SecurityIdentifier]::new([Security.Principal.WellKnownSidType]::BuiltinAdministratorsSid, $null),
    [Security.Principal.SecurityIdentifier]::new([Security.Principal.WellKnownSidType]::LocalSystemSid, $null)
)) {
    $rule = [Security.AccessControl.FileSystemAccessRule]::new(
        $sid,
        [Security.AccessControl.FileSystemRights]::FullControl,
        $inheritance,
        $propagation,
        $allow
    )
    $directorySecurity.AddAccessRule($rule)
}
Set-Acl -LiteralPath $liveRoot -AclObject $directorySecurity

Copy-Item -Path (Join-Path $stockConfig '*') -Destination $liveConfig -Recurse -Force

$configFile = Join-Path $liveConfig 'sunshine.conf'
$privateKey = Join-Path $liveConfig 'credentials\cakey.pem'
$certificate = Join-Path $liveConfig 'credentials\cacert.pem'
$stateFile = Join-Path $liveConfig 'sunshine_state.json'
$appsFile = Join-Path $liveConfig 'apps.json'
$sunshineLog = Join-Path $liveConfig 'foundation.sunshine.log'
$arguments = @(
    ('"' + $configFile + '"'),
    "port=$Port",
    'upnp=disabled',
    "pkey=$privateKey",
    "cert=$certificate",
    "file_state=$stateFile",
    "credentials_file=$stateFile",
    "file_apps=$appsFile",
    "log_path=$sunshineLog"
)
$process = Start-Process -FilePath $foundationExe `
    -ArgumentList $arguments `
    -WorkingDirectory $StageRoot `
    -RedirectStandardOutput $stdoutPath `
    -RedirectStandardError $stderrPath `
    -PassThru

[pscustomobject]@{
    Elevated = $true
    ProcessId = $process.Id
    Port = $Port
    LiveRoot = $liveRoot
    ConfigFile = $configFile
    SunshineLog = $sunshineLog
    StdoutLog = $stdoutPath
    StderrLog = $stderrPath
    StartedAt = (Get-Date).ToUniversalTime().ToString('o')
} | ConvertTo-Json | Set-Content -LiteralPath $ResultPath -Encoding UTF8
