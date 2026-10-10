<#
.SYNOPSIS
  The guest joins the running host again (the host copies the code, the guest pastes it and presses Join), and waits until the
  host's hot-join resync round has finished. For the system test, in a session started with FEVERLINK3_DEV=1 (the window has
  the DEV run row on top, which moves every control 24 pixels down).
#>
param([int]$Host_ = 0, [int]$Guest = 0, [int]$Dy = 24)
. "$PSScriptRoot\ui.ps1"
if ($Host_ -eq 0) {
    $ids = (Get-Content (Join-Path $env:TEMP 'fl3-pids.txt')).Split(' ')
    $Host_ = [int]$ids[0]; $Guest = [int]$ids[1]
}
$base = Join-Path $env:LOCALAPPDATA 'FeverLink3'
function Status($f) { $p = Join-Path (Join-Path $base $f) 'ctl_status.txt'; if (Test-Path $p) { try { [IO.File]::ReadAllText($p) } catch { '' } } else { '' } }

Click-Win $Host_ 409 (387 + $Dy)               # Copy code (host window open)
Start-Sleep 1
Click-Win $Guest 495 (455 + $Dy)               # the join code field (guest window open, idle)
Start-Sleep -Milliseconds 500
Send-Chord 0x56
Start-Sleep 1
Click-Win $Guest 393 (476 + $Dy)               # Join
$end = (Get-Date).AddSeconds(60)
$joined = $false
while ((Get-Date) -lt $end) { if ((Status 'live1') -match '\["role"\]="player"') { $joined = $true; break }; Start-Sleep -Milliseconds 500 }
Write-Host ("joined: {0}" -f $joined)
$seen = $false
$end = (Get-Date).AddSeconds(150)
while ((Get-Date) -lt $end) {
    $t = Status 'live1'
    if ($t -match '\["resync"\]="(Pausing|Saving|Transferring|Loading|Verifying)"') { $seen = $true }
    if ($seen -and $t.Contains('["resync"]="Idle"')) { break }
    Start-Sleep 1
}
Write-Host ("hot-join round seen: {0}" -f $seen)
