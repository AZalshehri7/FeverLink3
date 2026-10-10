<#
.SYNOPSIS
  Puts the game's window settings (windowed, size) back to what they were before the demo scripts changed them.
  Close the game first; the original file was kept by demo_setup.ps1 in %USERPROFILE%\fl3-backup\settings.lua.orig.
#>
$ErrorActionPreference = 'Stop'
$backup = Join-Path $env:USERPROFILE 'fl3-backup\settings.lua.orig'
if (-not (Test-Path $backup)) { Write-Host 'no backup found: nothing to restore'; exit 0 }
if (Get-Process TransportFever3 -ErrorAction SilentlyContinue) { Write-Host 'close Transport Fever 3 first'; exit 1 }
$steam = Join-Path ${env:ProgramFiles(x86)} 'Steam\userdata'
$file = Get-ChildItem $steam -Directory | ForEach-Object { Join-Path $_.FullName '3493540\local\settings.lua' } | Where-Object { Test-Path $_ } | Select-Object -First 1
Copy-Item $backup $file -Force
Write-Host "restored $file"
