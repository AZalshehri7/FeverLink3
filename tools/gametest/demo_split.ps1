<#
.SYNOPSIS
  The split-screen demo for the project documentation: two real Transport Fever 3 games side by side (LEFT = host
  "Anna", RIGHT = guest "Ben"), each starting in its own world. The host hosts and copies the join code, the guest
  pastes it and joins, the host starts the session (the host's world is sent; the guest's game reloads into it while the host keeps playing), then
  each side gives commands (speed, pause, chat) and the other game shows them. A picture of the whole screen is
  taken all the way through and assembled into an animated GIF.

.DESCRIPTION
  Needs the release build (cargo build --release --workspace) and Pillow for the GIF (pip install pillow).
  The names are neutral on purpose (no Windows or Steam name is typed or shown). Both games use the developer
  connection on the loopback address, because two games of one Steam account cannot reach each other through Steam.
  The games are left running at the end; restore_settings.ps1 puts the game's window settings back.

  -NoSetup -Left PID -Right PID   skip starting the games (they must be in two different worlds, mod active on the right)
  -Out DIR                        where frames and the GIF go (default %TEMP%\fl3-demo)
#>
param(
    [switch]$NoSetup,
    [int]$Left = 0,
    [int]$Right = 0,
    [string]$Out = (Join-Path $env:TEMP 'fl3-demo'),
    [string]$Python = (Join-Path $env:LOCALAPPDATA 'Programs\Python\Python312\python.exe')
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\demo_setup.ps1"

$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Frames = Join-Path $Out 'frames'
$script:n = 0
$Utf8 = New-Object Text.UTF8Encoding $false

# --- where things are in a game window (client pixels), measured in the finished windows
$Ui = @{
    HostButton = @(415, 413)    # idle window: Host a game
    CopyCode   = @(409, 387)    # hosting window: Copy code
    CodeField  = @(495, 455)    # idle window: the join code field
    JoinButton = @(393, 476)    # idle window: Join
    StartHost  = @(422, 476)    # hosting window with two players: Start the session
    HostPause  = @(398, 555); HostX1 = @(432, 555); HostX2 = @(456, 555)     # started host window: speed row
    GuestPause = @(398, 491); GuestX1 = @(432, 491); GuestX2 = @(456, 491)   # started guest window: speed row
    GuestChat  = @(546, 511); GuestSend = @(396, 531)
    HostChat   = @(489, 587); HostSend  = @(395, 608)                        # after one chat line
}

# --- pictures
function Snap([string]$Caption, [double]$Seconds = 0.5, $Mouse = $null) {
    $script:n++
    $name = 'f{0:0000}.png' -f $script:n
    Save-Region (Join-Path $Frames $name) 0 0 2560 1396 1600
    $mx = ''; $my = ''
    if ($Mouse) { $mx = [string]$Mouse[0]; $my = [string]$Mouse[1] }
    $line = ("{0}`t{1}`t{2}`t{3}`t{4}`n" -f $name, $Caption, $Seconds, $mx, $my)
    [IO.File]::AppendAllText((Join-Path $Frames 'frames.tsv'), $line, $Utf8)
}

function Hold([double]$Seconds, [string]$Caption, [int]$PauseMs = 200) {
    $end = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $end) { Snap $Caption 0.5; Start-Sleep -Milliseconds $PauseMs }
}

# Moves the pointer to a place in a game window, shows it in a picture, then clicks.
function Press([int]$Id, $At, [string]$Caption) {
    Focus-Game $Id
    $a = Get-ClientArea $Id
    $sx = $a.X + $At[0]; $sy = $a.Y + $At[1]
    [Fl3.Win]::SetCursorPos($sx, $sy) | Out-Null
    Start-Sleep -Milliseconds 250
    Snap $Caption 0.8 @($sx, $sy)
    [Fl3.Win]::mouse_event(2, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 80
    [Fl3.Win]::mouse_event(4, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 400
}

function Write-Into([string]$Text, [string]$Caption) {
    Send-Text $Text
    Start-Sleep -Milliseconds 300
    Hold 1.0 $Caption
}

# --- what the in-game module says about itself (the status file of a game's live folder)
function Live-Text([string]$Folder) {
    $f = Join-Path (Join-Path $env:LOCALAPPDATA 'FeverLink3') (Join-Path $Folder 'ctl_status.txt')
    if (Test-Path $f) { try { [IO.File]::ReadAllText($f) } catch { '' } } else { '' }
}

# The window model the game's mod keeps for its own window (hud_state.txt): the pause stamp is in there.
function Hud-Text([string]$Folder) {
    $f = Join-Path (Join-Path $env:LOCALAPPDATA 'FeverLink3') (Join-Path $Folder 'hud_state.txt')
    if (Test-Path $f) { try { [IO.File]::ReadAllText($f) } catch { '' } } else { '' }
}

function Wait-Live([string]$Folder, [scriptblock]$Test, [int]$Seconds, [string]$Caption) {
    $end = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $end) {
        if (& $Test (Live-Text $Folder)) { return $true }
        Snap $Caption 0.5
        Start-Sleep -Milliseconds 300
    }
    Write-Warning "timed out waiting: $Caption"
    return $false
}

function Check([bool]$Ok, [string]$What) {
    if ($Ok) { Demo-Step "ok: $What" } else { Write-Warning "NOT as expected: $What" }
}

# ---------------------------------------------------------------------------------------------------- the show
New-Item -ItemType Directory -Force $Frames | Out-Null
Get-ChildItem $Frames -ErrorAction SilentlyContinue | ForEach-Object { [IO.File]::Delete($_.FullName) }

if (-not $NoSetup) {
    $ids = Demo-Setup
    $Left = $ids.Left; $Right = $ids.Right
}
Demo-Step "left pid $Left (host), right pid $Right (guest)"

Hold 3.0 'Two copies of Transport Fever 3, each with its own world'

# the host
Press $Left $Pos.BarButton 'HOST: open the FeverLink3 window in the game bar'
Start-Sleep 3     # no picture while the name field still holds the Steam name: the name is typed before the next one
Write-Into 'Anna' 'HOST: type your name'
Press $Left $Ui.HostButton 'HOST: press Host a game'
$ok = Wait-Live 'live1' { param($t) $t.Contains('["state"]="hosting"') } 30 'HOST: starting to host...'
Check $ok 'the host is hosting'
Hold 2.0 'HOST: the window shows a join code'
Press $Left $Ui.CopyCode 'HOST: Copy code puts the code on the clipboard'
Hold 2.0 'HOST: copied, ready to send to a friend'

# the guest
Press $Right $Pos.BarButton 'GUEST: open the FeverLink3 window'
Start-Sleep 3
Write-Into 'Ben' 'GUEST: type your name'
Press $Right $Ui.CodeField 'GUEST: click the join code field'
Send-Chord 0x56
Hold 1.5 'GUEST: paste the join code (Ctrl+V)'
Press $Right $Ui.JoinButton 'GUEST: press Join'
$ok = Wait-Live 'live1' { param($t) $t.Contains('["name"]="Ben"') } 40 'GUEST: connecting to the host...'
Check $ok 'the host sees the guest'
Hold 2.5 'Connected: the host sees Ben, the guest sees Anna'

# start: the host's world goes to the guest
Press $Left $Ui.StartHost 'HOST: press Start the session'
$seen = $false
$end = (Get-Date).AddSeconds(150)
while ((Get-Date) -lt $end) {
    $t = Live-Text 'live1'
    if ($t -match '\["resync"\]="(Pausing|Saving|Transferring|Loading|Verifying)"') { $seen = $true }
    if ($seen -and $t.Contains('["resync"]="Idle"')) { break }
    Snap "The host's world is sent to the guest, whose game loads it; the host keeps playing (sped up)" 0.5
    Start-Sleep -Milliseconds 1200
}
Check $seen 'a resync round ran at Start'
Start-Sleep 4
Click-Win $Right $Pos.TutorialClose[0] $Pos.TutorialClose[1]    # the reload brought the tutorial panel back
Start-Sleep -Milliseconds 800
Hold 3.0 "Both games now play the host's world"

# the guest's reload closed its window: open it again (the host's game never reloaded, its window stays)
Press $Right $Pos.BarButton 'GUEST: open the window again'
Hold 1.5 'Both windows show the running session'

# commands from each side, shown on the other
Press $Left $Ui.HostX2 'HOST: press 2x'
Hold 6.0 'The host set 2x: both games run at double speed, same date'
Check ((Live-Text 'live1').Contains('["speed"]=2')) 'the session runs at x2 after the host pressed 2x'
Press $Right $Ui.GuestPause 'GUEST: press Pause'
Hold 4.0 'The guest paused: both games stop, same date'
Check ((Hud-Text 'live1') -match '\["pauseAt"\]=\d') 'the session is paused after the guest pressed Pause'
Press $Right $Ui.GuestChat 'GUEST: type a message'
Write-Into 'Hello Anna' 'GUEST: type a message'
Press $Right $Ui.GuestSend 'GUEST: press Send'
Hold 2.5 'The host sees the guest message'
Press $Left $Ui.HostChat 'HOST: answer'
Write-Into 'Hi Ben' 'HOST: answer'
Press $Left $Ui.HostSend 'HOST: press Send'
Hold 2.5 'The guest sees the answer'
Press $Left $Ui.HostX1 'HOST: press 1x'
Hold 6.0 'Running again at 1x in both games, same date'
Check ((Live-Text 'live1').Contains('["speed"]=1')) 'the session runs at x1 after the host pressed 1x'

Demo-Step ("{0} pictures in {1}" -f $script:n, $Frames)
$gif = Join-Path $Out 'split-screen-demo.gif'
if (Test-Path $Python) {
    & $Python (Join-Path $PSScriptRoot 'make_demo_gif.py') $Frames $gif --width 1280 --colors 128 --webp (Join-Path $Out 'split-screen-demo.webp')
} else {
    Write-Warning "no Python at ${Python}: assemble the GIF later with make_demo_gif.py"
}
Demo-Step "done; the games stay open. GIF: $gif"
