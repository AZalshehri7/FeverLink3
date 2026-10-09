<#
.SYNOPSIS
  Two real Transport Fever 3 processes on this PC: the first hosts from the in-game controller, the second joins
  through `feverlink3 join --launch`. Passes when the save transfer and reload happened and both games report the
  same checksum parts at the same game times.

.DESCRIPTION
  Needs: Steam running, the game installed, and a savegame the main menu's "Continue" tile loads, with the mod
  activated in it (make one from a running game with the window's host mode and a `resync_save` line, or play a
  new game with the mod active and save). Takes about 4 minutes. Run from the repository root:
      powershell -ExecutionPolicy Bypass -File tools\gametest\two_games.ps1
  Output: a PASS/FAIL line, details and the logs' paths. The screen must be unlocked and nobody may use the mouse.
#>
param(
    [int]$LoadSeconds = 28,
    [int]$SettleSeconds = 60,
    [switch]$NoInstall
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
function Wait-Until([scriptblock]$Test, [int]$Seconds, [string]$What) {
    $until = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $until) { if (& $Test) { return } Start-Sleep -Milliseconds 500 }
    throw "timed out waiting for $What"
}
function Checksums([string]$Path) {
    $rows = @{}
    if (-not (Test-Path $Path)) { return $rows }
    foreach ($l in Get-Content $Path) {
        $f = $l -split "`t", 4
        if ($f.Count -eq 4 -and $f[1] -eq 'checksum_report') {
            $n = [regex]::Match($f[3], '\["n"\]=(\d+)').Groups[1].Value
            $parts = [regex]::Match($f[3], '\["parts"\]=(\{.*?\})').Groups[1].Value
            $time = [regex]::Match($f[3], '\["time"\]=(\d+)').Groups[1].Value
            if ($n) { $rows[$n] = @{ parts = $parts; time = $time } }
        }
    }
    return $rows
}

try {
    Get-Process TransportFever3, feverlink3 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 3
    if (-not $NoInstall) { Step 'install'; & $exe install | Out-Null }

    Step 'start game 1 (host)'
    & "${env:ProgramFiles(x86)}\Steam\steam.exe" -applaunch 3493540
    Start-Sleep 45
    $g1 = (Get-Process TransportFever3 | Sort-Object StartTime | Select-Object -First 1).Id
    Click-Game $g1 $Fl3.MenuContinue[0] $Fl3.MenuContinue[1]
    Start-Sleep $LoadSeconds
    Click-Game $g1 $Fl3.StartGame[0] $Fl3.StartGame[1]
    Wait-Until { (Read-Status) -match '"state"\]="idle"' } 60 'the controller of game 1'
    Start-Sleep 5

    Step 'host and start the session'
    Send-Ctl 'host' '{["port"]=28090,["transport"]="tcp"}'
    Wait-Until { (Read-Status) -match '\["state"\]="hosting"' } 30 'hosting'
    Send-Ctl 'start'
    Wait-Until { (Read-Status) -match '\["started"\]=true' } 30 'the session to start'
    $code = [regex]::Match((Read-Status), '\["code"\]="([A-Z2-7]+)"').Groups[1].Value

    Step 'start game 2 (join)'
    $out = Join-Path $shots 'join.txt'
    $err = Join-Path $shots 'join_err.txt'
    Remove-Item $out, $err -ErrorAction SilentlyContinue
    Start-Process $exe -ArgumentList 'join', $code, '--name', 'Player2', '--launch' -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden | Out-Null
    # the launcher prints its own lines on stdout and the session log on stderr
    function Joined-Text { (@($out, $err) | Where-Object { Test-Path $_ } | ForEach-Object { Get-Content $_ -Raw }) -join "`n" }
    Wait-Until { (Joined-Text) -match 'game started \(pid (\d+)\)' } 60 'game 2 to start'
    $g2 = [int][regex]::Match((Joined-Text), 'game started \(pid (\d+)\)').Groups[1].Value
    Wait-Until { (Joined-Text) -match 'save 1 received' } 90 'the save to arrive'
    Step 'save received; load a world in game 2'
    Start-Sleep 20
    Click-Game $g2 $Fl3.MenuContinue[0] $Fl3.MenuContinue[1]
    Start-Sleep $LoadSeconds
    Click-Game $g2 $Fl3.StartGame[0] $Fl3.StartGame[1]

    Step "settle ($SettleSeconds s)"
    Start-Sleep $SettleSeconds
    Save-Screenshot (Join-Path $shots 'final.png')

    $p2dir = Get-ChildItem (Join-Path $env:TEMP 'feverlink3') -Directory -Filter 'Player2-*' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    $a = Checksums (Join-Path $live 'from_game.log')
    $b = Checksums (Join-Path $p2dir.FullName 'from_game.log')
    $common = @($a.Keys | Where-Object { $b.ContainsKey($_) } | Sort-Object { [int]$_ })
    $same = @($common | Where-Object { $a[$_].parts -eq $b[$_].parts -and $a[$_].time -eq $b[$_].time })
    $reloaded = [bool]((Get-Content (Join-Path $p2dir.FullName 'mod.log') -ErrorAction SilentlyContinue) -match 'reloaded after a resync')
    Write-Host ''
    Write-Host ("rounds compared: {0}, identical: {1}, joiner reloaded after resync: {2}" -f $common.Count, $same.Count, $reloaded)
    $verdict = 1
    if ($common.Count -ge 2 -and $same.Count -eq $common.Count -and $reloaded) {
        Write-Host 'PASS' -ForegroundColor Green
        $verdict = 0
    } else {
        Write-Host 'FAIL' -ForegroundColor Red
        foreach ($n in $common) {
            if ($a[$n].parts -ne $b[$n].parts) { Write-Host "  round $n differs:`n    host   $($a[$n].parts)`n    player $($b[$n].parts)" }
        }
    }
    Write-Host "host logs:   $live"
    Write-Host "player logs: $($p2dir.FullName)"
    Write-Host "screenshot:  $shots\final.png"
    exit $verdict
}
catch {
    Write-Host "ERROR: $_" -ForegroundColor Red
    try { Save-Screenshot (Join-Path $shots 'error.png') } catch {}
    exit 2
}
