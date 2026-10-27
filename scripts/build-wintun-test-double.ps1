param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $taskRoot 'target\wintun-test-double' }
$taskOutput = [IO.Path]::GetFullPath($OutputDirectory)
$taskTarget = [IO.Path]::GetFullPath((Join-Path $taskRoot 'target')) + [IO.Path]::DirectorySeparatorChar
if (-not $taskOutput.StartsWith($taskTarget, [StringComparison]::OrdinalIgnoreCase)) { throw 'Test build output must be inside this repository target directory.' }
New-Item -ItemType Directory -Path $taskOutput -Force | Out-Null
$taskVsWhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$taskVs = & $taskVsWhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $taskVs) { throw 'MSVC C/C++ build tools were not found.' }
$taskTools = Get-ChildItem (Join-Path $taskVs 'VC\Tools\MSVC') -Directory | Sort-Object Name -Descending | Select-Object -First 1
$taskKit = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
$taskVersion = Get-ChildItem (Join-Path $taskKit 'Include') -Directory | Where-Object { $_.Name -match '^10\.' } | Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
if (-not $taskVersion) { throw 'The Windows SDK was not found.' }
$taskCompilerDir = Join-Path $taskTools.FullName 'bin\Hostx64\x64'
$taskCompiler = Join-Path $taskCompilerDir 'cl.exe'
$taskSource = Join-Path $taskRoot 'tests\support\wintun-test-double.c'
$taskDll = Join-Path $taskOutput 'wincan-test-double.dll'
$taskArgs = @('/nologo', '/LD', '/MT', '/O2', '/W4', '/WX',
    ('/I' + (Join-Path $taskTools.FullName 'include')),
    ('/I' + (Join-Path $taskVersion.FullName 'um')),
    ('/I' + (Join-Path $taskVersion.FullName 'shared')),
    ('/I' + (Join-Path $taskVersion.FullName 'ucrt')),
    ('/Fo' + (Join-Path $taskOutput 'test-double.obj')),
    ('/Fd' + (Join-Path $taskOutput 'test-double.pdb')),
    $taskSource, '/link',
    ('/LIBPATH:' + (Join-Path $taskTools.FullName 'lib\x64')),
    ('/LIBPATH:' + (Join-Path $taskKit ('Lib\' + $taskVersion.Name + '\um\x64'))),
    ('/LIBPATH:' + (Join-Path $taskKit ('Lib\' + $taskVersion.Name + '\ucrt\x64'))),
    ('/OUT:' + $taskDll), ('/IMPLIB:' + (Join-Path $taskOutput 'test-double.lib')), 'kernel32.lib')
$taskOldPath = $env:PATH
try {
    $env:PATH = $taskCompilerDir + ';' + $env:PATH
    & $taskCompiler @taskArgs
    if ($LASTEXITCODE -ne 0) { throw "Test DLL compiler failed ($LASTEXITCODE)." }
} finally { $env:PATH = $taskOldPath }
Write-Output $taskDll
