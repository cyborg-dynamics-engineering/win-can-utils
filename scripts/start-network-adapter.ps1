#Requires -RunAsAdministrator
param(
    [Parameter(Mandatory = $true)][string]$Dll,
    [string]$Interface = 'vcan0',
    [string]$Adapter,
    [string]$Local = '198.18.0.1',
    [string]$Peer = '198.18.0.2',
    [int]$Port = 29536
)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskBinary = Join-Path $taskRoot 'target\debug\canadapter.exe'
if (-not (Test-Path -LiteralPath $taskBinary)) { throw 'Build first with cargo build --bins --locked --offline.' }
if (-not $Adapter) { $Adapter = 'WinCAN ' + $Interface }
$taskArgs = @('run', '--dll', $Dll, '--interface', $Interface, '--adapter', $Adapter,
    '--local', $Local, '--peer', $Peer, '--port', $Port, '--create', '--configure-ip')
& $taskBinary @taskArgs
exit $LASTEXITCODE
