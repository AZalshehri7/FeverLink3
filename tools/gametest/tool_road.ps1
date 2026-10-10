<#
.SYNOPSIS
  Draws one road with the game's own road tool in one game: opens the tool, clicks the start and the end point (client
  pixels of that game's window), optionally takes a picture of the preview, and leaves the tool with Escape.
.EXAMPLE
  tool_road.ps1 -ProcId 1234 -X0 690 -Y0 1074 -X1 807 -Y1 1098 -Shot preview
#>
param(
    [Parameter(Mandatory)][int]$ProcId,
    [Parameter(Mandatory)][int]$X0, [Parameter(Mandatory)][int]$Y0,
    [Parameter(Mandatory)][int]$X1, [Parameter(Mandatory)][int]$Y1,
    [string]$Shot = '',
    [switch]$NoLeave
)
. "$PSScriptRoot\ui.ps1"

function Press-Escape { Send-Key 0x1B $true; Start-Sleep -Milliseconds 100; Send-Key 0x1B $false; Start-Sleep -Milliseconds 500 }

Focus-Game $ProcId
Press-Escape; Press-Escape                    # whatever tool or panel was open
Click-Win $ProcId 632 1311                    # the roads tool in the toolbar
Start-Sleep 2
# the tool computes its preview some time after the pointer arrived: a click right after the move is ignored
function Hover-Click([int]$X, [int]$Y) {
    $a = Get-ClientArea $ProcId
    [Fl3.Win]::SetCursorPos($a.X + $X, $a.Y + $Y) | Out-Null
    Start-Sleep -Milliseconds 1100
    [Fl3.Win]::mouse_event(2, 0, 0, 0, 0); Start-Sleep -Milliseconds 80; [Fl3.Win]::mouse_event(4, 0, 0, 0, 0)
}
Hover-Click $X0 $Y0
Start-Sleep 1
Hover-Click $X1 $Y1
Start-Sleep 2
if ($Shot) {
    $a = Get-ClientArea $ProcId
    Save-Region (Join-Path $env:TEMP ("fl3-gametest\" + $Shot + ".png")) ($a.X - 8) 0 1280 1396 1100
}
if (-not $NoLeave) { Press-Escape; Press-Escape }
