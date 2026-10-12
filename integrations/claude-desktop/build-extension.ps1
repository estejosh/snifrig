# Builds snifrig.mcpb (Claude Desktop Extension).
# Usage: .\build-extension.ps1 [-Exe path\to\snifrig-mcp.exe]
param(
  [string]$Exe = ''
)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
if (-not $Exe) {
  $cands = @("$here\..\..\target\release\snifrig-mcp.exe", "$here\..\..\target-mcp\release\snifrig-mcp.exe", "$env:LOCALAPPDATA\snifrig\bin\snifrig-mcp.exe")
  $Exe = $cands | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Exe -or -not (Test-Path $Exe)) { throw 'snifrig-mcp.exe not found. Build it first or pass -Exe.' }
New-Item -ItemType Directory -Force "$here\server" | Out-Null
Copy-Item $Exe "$here\server\snifrig-mcp.exe" -Force
if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
  Write-Host 'node not found: exe copied to server\, but cannot pack. Install Node.js, then run: npx -y @anthropic-ai/mcpb pack'
  exit 1
}
Push-Location $here
try {
  # Pack only what the extension needs (a clean staging folder keeps the .mcpb small).
  $stage = Join-Path $env:TEMP 'snifrig-mcpb-stage'
  Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory $stage\server | Out-Null
  Copy-Item manifest.json, icon.png $stage
  Copy-Item server\snifrig-mcp.exe $stage\server
  npx -y @anthropic-ai/mcpb pack $stage "$here\snifrig.mcpb"
  if (Test-Path "$here\snifrig.mcpb") { Write-Host "Built $here\snifrig.mcpb" } else { throw 'pack did not produce snifrig.mcpb' }
} finally { Pop-Location }
