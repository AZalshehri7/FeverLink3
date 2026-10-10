<#
.SYNOPSIS
  Presses Escape in one game (the game's pause menu), optionally clicks a point of it, and saves a picture of the whole window.
#>
param(
    [Parameter(Mandatory)][int]$ProcId,
    [int]$X = -1, [int]$Y = -1,
    [string]$Shot = 'esc'
)
. "$PSScriptRoot\ui.ps1"
Focus-Game $ProcId
if ($X -lt 0) {
    Send-Key 0x1B $true; Start-Sleep -Milliseconds 100; Send-Key 0x1B $false
    Start-Sleep 2
} else {
    Click-Win $ProcId $X $Y
    Start-Sleep 3
}
$a = Get-ClientArea $ProcId
Save-Region (Join-Path $env:TEMP ("fl3-gametest\" + $Shot + ".png")) ($a.X - 8) 0 1280 1396 1000
