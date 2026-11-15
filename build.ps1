# Сборка Phoenix Torrent. Запуск: powershell -ExecutionPolicy Bypass -File build.ps1
# Каталог сборки вынесен в короткий путь: линкер Windows не работает с путями длиннее 260 символов.
$ErrorActionPreference = 'Stop'
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = "$env:USERPROFILE\.phoenix-build" }

Push-Location "$PSScriptRoot\src-tauri"
try {
    cargo build --release --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw "Сборка не удалась" }
} finally {
    Pop-Location
}

Copy-Item "$env:CARGO_TARGET_DIR\release\phoenix-torrent.exe" "$PSScriptRoot\PhoenixTorrent.exe" -Force
Write-Host "Готово: $PSScriptRoot\PhoenixTorrent.exe"
