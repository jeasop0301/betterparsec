param(
    [Parameter(Mandatory = $true)]
    [string] $FoundationExe,
    [Parameter(Mandatory = $true)]
    [string] $CompletionSentinel,
    [int] $DelaySeconds = 600
)

Start-Sleep -Seconds $DelaySeconds

if (Test-Path -LiteralPath $CompletionSentinel) {
    exit 0
}

Get-CimInstance Win32_Process |
    Where-Object { $_.ExecutablePath -eq $FoundationExe } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -ErrorAction SilentlyContinue }

Start-Service -Name SunshineService -ErrorAction SilentlyContinue
