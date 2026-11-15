# Open a project's CSV export directory in Windows Explorer.
param([string]$ProjectPath = (Join-Path $PSScriptRoot '..\test-project'))

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath $ProjectPath).Path
if (-not (Test-Path -LiteralPath (Join-Path $projectRoot 'not-bad-query.toml') -PathType Leaf)) {
    throw "Not a not-bad-query project: $projectRoot"
}
$exportsPath = Join-Path $projectRoot 'exports'
[IO.Directory]::CreateDirectory($exportsPath) | Out-Null
Invoke-Item -LiteralPath $exportsPath
