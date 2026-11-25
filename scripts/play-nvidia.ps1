# Runs the game with the settings measured fastest on an RTX 5070 (i5-14400F):
# 6.5-7.1 fps in the prison prologue, against 2.6-4.8 with the defaults
# (2026-11-25). The game folder is only read; caches and saves go to -Cache
# and -Data, which must lie outside it.
param(
    [Parameter(Mandatory = $true)][string]$Game,
    [string]$Cache = "D:\VIctory-PC\vfork-cache",
    [string]$Data = "D:\VIctory-PC\vfork-data",
    [string]$Log = "$env:TEMP\victory-nvidia.log",
    [string]$Exe = "$PSScriptRoot\..\target\release\victory.exe"
)
New-Item -ItemType Directory -Force $Cache, $Data | Out-Null
$env:VICTORY_CACHE_DIR = $Cache
$env:VICTORY_DATA_DIR = $Data
# victory-fork's speed switches (see the README's frame-rate notes):
$env:VICTORY_ASYNC_SUBMIT = "1"     # a thread of the layer submits, not the title's render thread
$env:VICTORY_QUEUE2 = "1"           # fetches of shader writes on a copy queue
$env:VICTORY_INDIRECT_WINDOW = "1"  # indirect draws in windows (off again at the first driver error)
$env:VICTORY_TEXTURE_NO_WAIT = "1"  # texture refreshes do not wait for remaps
$env:VICTORY_COPY_PLAIN = "1"       # copies of title memory made in place
& $Exe run $Game --execute *> $Log
Write-Host "Exited with $LASTEXITCODE; log: $Log"
