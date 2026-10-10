<#
.SYNOPSIS
  Steam checks that need only ONE account: the game connects to its own Steam ID through Steam's networking, and the
  transport's built-in loopback self-check runs (connect, round trips, chunk-sized frames, the biggest frame, close).

.DESCRIPTION
  Needs: Steam running, the game installed, a savegame the main menu's "Continue" tile loads. Takes about 3 minutes
  and uses the mouse and screen, so do not touch the PC meanwhile. Run from the repository root:
      powershell -ExecutionPolicy Bypass -File tools\gametest\steam_one_account.ps1
  Prints the Steam part of the controller's status after each step. Closes the game at the end unless -KeepRunning.
#>
param(
    [switch]$NoInstall,
    [switch]$KeepRunning
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\ui.ps1"
$root = Resolve-Path "$PSScriptRoot\..\.."
$exe = Join-Path $root 'target\release\feverlink3.exe'
$live = Join-Path $env:LOCALAPPDATA 'FeverLink3\live'
$shots = Join-Path $env:TEMP 'fl3-gametest'
New-Item -ItemType Directory -Force $shots | Out-Null

function Step($text) { Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $text) }
function Send-Ctl([string]$Kind, [string]$Payload = '{}') {
    $file = Join-Path $live 'ctl_cmd.log'
    $seq = 1
    if (Test-Path $file) { $seq = (Get-Content $file | Measure-Object).Count + 1 }
    Add-Content -Path $file -Value ("{0}`t{1}`t0`t{2}" -f $seq, $Kind, $Payload) -Encoding ascii
}
function Read-Status { Get-Content (Join-Path $live 'ctl_status.txt') -Raw -ErrorAction SilentlyContinue }
function Steam-Part { $s = Read-Status; $i = $s.IndexOf('["steam"]='); if ($i -ge 0) { $s.Substring($i, [Math]::Min(1800, $s.Length - $i)) } else { '(no steam part yet)' } }
function Wait-Until([scriptblock]$Test, [int]$Seconds, [string]$What) {
    $until = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $until) { if (& $Test) { return } Start-Sleep -Milliseconds 500 }
    throw "timed out waiting for $What"
}

try {
    Get-Process TransportFever3, feverlink3 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 3
    if (-not $NoInstall) { Step 'install'; & $exe install | Out-Null }

    Step 'start the game'
    & "${env:ProgramFiles(x86)}\Steam\steam.exe" -applaunch 3493540
    Start-Sleep 45
    $g = (Get-Process TransportFever3 | Sort-Object StartTime | Select-Object -First 1).Id
    Click-Game $g $Fl3.MenuContinue[0] $Fl3.MenuContinue[1]
    Start-Sleep 28
    Click-Game $g $Fl3.StartGame[0] $Fl3.StartGame[1]
    Wait-Until { (Read-Status) -match '\["id"\]="\d{16,20}"' } 90 'the Steam ID in the status'
    Start-Sleep 5
    $id = [regex]::Match((Read-Status), '\["steam"\]=.*?\["id"\]="(\d+)"').Groups[1].Value
    Step "own Steam ID: $id"
    Write-Host (Steam-Part)

    Step 'check 1: connect to our own Steam ID'
    Send-Ctl 'steamtest' ('{["id"]="' + $id + '"}')
    Start-Sleep 25
    Write-Host (Steam-Part)

    Step 'check 2: the transport self-check (loop)'
    Send-Ctl 'steamtest' '{["id"]="loop"}'
    Start-Sleep 30
    Write-Host (Steam-Part)
    Step 'check 3: Host a game over Steam (the default transport), then open the window'
    Send-Ctl 'host' '{}'
    Wait-Until { (Read-Status) -match '\["state"\]="hosting"' } 40 'hosting over Steam'
    Start-Sleep 3
    $st = Read-Status
    $code = [regex]::Match($st, '\["code"\]="([A-Z2-7]+)"').Groups[1].Value
    Write-Host ("hosting: transport={0} steamid={1} code length={2} players={3}" -f `
        [regex]::Match($st, '\["transport"\]="(\w+)"').Groups[1].Value, `
        [regex]::Match($st, '\["steamid"\]="?(\d+)').Groups[1].Value, $code.Length, `
        ([regex]::Matches($st, '\["role"\]=').Count))
    Click-Game $g $Fl3.BarButton[0] $Fl3.BarButton[1]
    Start-Sleep 4
    Save-Screenshot (Join-Path $shots 'steam_host_window.png')
    Write-Host "window screenshot: $shots\steam_host_window.png"
    Save-Screenshot (Join-Path $shots 'steam_one.png')
    Write-Host "screenshot: $shots\steam_one.png"
}
catch {
    Write-Host "ERROR: $_" -ForegroundColor Red
    try { Save-Screenshot (Join-Path $shots 'steam_one_error.png') } catch {}
}
finally {
    if (-not $KeepRunning) { Get-Process TransportFever3, feverlink3 -ErrorAction SilentlyContinue | Stop-Process -Force }
}
