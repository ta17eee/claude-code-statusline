$ErrorActionPreference = "Stop"

$repo = "ta17eee/claude-code-statusline"
$claudeDir = Join-Path $HOME ".claude"
$dest = Join-Path $claudeDir "statusline.exe"

if (-not (Test-Path $claudeDir)) {
    New-Item -ItemType Directory -Path $claudeDir | Out-Null
}

$url = "https://github.com/$repo/releases/latest/download/statusline-windows-x86_64.exe"

Write-Host "Downloading statusline-windows-x86_64.exe..."
Invoke-WebRequest -Uri $url -OutFile $dest

Write-Host "Installed to $dest"
Write-Host ""
Write-Host "Add this to `%USERPROFILE%\.claude\settings.json`:"
Write-Host "  `"statusLine`": { `"type`": `"command`", `"command`": `"$($dest -replace '\\', '\\\\')`" }"
