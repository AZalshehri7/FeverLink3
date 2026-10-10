<#
.SYNOPSIS
  Demolishes whatever is under a point with the game's bulldozer: opens the tool, hovers, clicks, confirms nothing else, leaves
  the tool. Client pixels of that game's window. -Shot saves a picture of the hover (the preview of what would go).
#>
param(
    [Parameter(Mandatory)][int]$ProcId,
    [Parameter(Mandatory)][int]$X, [Parameter(Mandatory)][int]$Y,
    [string]$Shot = ''
)
. "$PSScriptRoot\ui.ps1"

function Press-Escape { Send-Key 0x1B $true; Start-Sleep -Milliseconds 100; Send-Key 0x1B $false; Start-Sleep -Milliseconds 500 }

Focus-Game $ProcId
Press-Escape; Press-Escape
Click-Win $ProcId 855 1314                    # the bulldozer in the toolbar (right of the trophy and the extras)
Start-Sleep 2
$a = Get-ClientArea $ProcId
[Fl3.Win]::SetCursorPos($a.X + $X, $a.Y + $Y) | Out-Null
Start-Sleep -Milliseconds 1500                # the preview of what would be demolished
if ($Shot) { Save-Region (Join-Path $env:TEMP ("fl3-gametest\" + $Shot + ".png")) ($a.X - 8) 500 1280 700 1100 }
[Fl3.Win]::mouse_event(2, 0, 0, 0, 0); Start-Sleep -Milliseconds 80; [Fl3.Win]::mouse_event(4, 0, 0, 0, 0)
Start-Sleep 2
Press-Escape; Press-Escape
