# Builds the release and stages the two downloads in release-assets\:
#   ClipCompressionCompanion-Setup.exe     the NSIS installer (per-user, no admin)
#   ClipCompressionCompanion-Portable.zip  the app and FFmpeg side by side, nothing to install
# Run: npm run package
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

npm run build
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$version = (Get-Content src-tauri\tauri.conf.json -Raw | ConvertFrom-Json).version
$release = "src-tauri\target\release"
$out = "release-assets"
New-Item -ItemType Directory -Force $out | Out-Null

$setup = Get-ChildItem "$release\bundle\nsis\*_${version}_*-setup.exe" | Select-Object -First 1
Copy-Item $setup.FullName "$out\ClipCompressionCompanion-Setup.exe" -Force

$stage = Join-Path ([IO.Path]::GetTempPath()) "ccc-portable"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null
Copy-Item "$release\clip-compression-companion.exe" "$stage\ClipCompressionCompanion.exe"
Copy-Item "$release\ffmpeg.exe" "$stage\ffmpeg.exe"
Copy-Item LICENSE "$stage\LICENSE.txt"
Copy-Item src-tauri\FFMPEG-NOTICE.txt "$stage\FFMPEG-NOTICE.txt"
Compress-Archive -Path "$stage\*" -DestinationPath "$out\ClipCompressionCompanion-Portable.zip" -CompressionLevel Optimal -Force
Remove-Item $stage -Recurse -Force

Get-ChildItem $out | ForEach-Object { "{0,-42} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB) }
