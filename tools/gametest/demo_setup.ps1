<#
.SYNOPSIS
  Two real Transport Fever 3 games side by side on one screen, each in its own world: the LEFT game loads the saved
  green map ("Continue"), the RIGHT game starts a new desert map. Both have the in-game FeverLink3 window available
  and both use the developer connection (loopback), because two games of one Steam account cannot reach each other
  through Steam. The games are left running.

.DESCRIPTION
  Dot-source this file (the demo script does) or run it alone. It changes the game's window mode to windowed for the
  recording (the original settings file is kept in %USERPROFILE%\fl3-backup\settings.lua.orig; restore_settings.ps1
  puts it back) and installs the build the way a player does (Install-Overlay).
#>
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\ui.ps1"

$Demo = @{
    Game     = Join-Path ${env:ProgramFiles(x86)} 'Steam\steamapps\common\Transport Fever 3'
    Settings = Join-Path ${env:ProgramFiles(x86)} 'Steam\userdata'
    Backup   = Join-Path $env:USERPROFILE 'fl3-backup\settings.lua.orig'
    OuterH   = 1385      # window height, so that the bottom bar clears the taskbar
}

function Demo-Step($text) { Write-Host ("[{0:HH:mm:ss}] {1}" -f (Get-Date), $text) }

function Demo-SettingsFile {
    # the user's settings file of Transport Fever 3 (app 3493540): the first Steam user folder that has one
    Get-ChildItem $Demo.Settings -Directory | ForEach-Object { Join-Path $_.FullName '3493540\local\settings.lua' } |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}

function Demo-PatchSettings {
    $file = Demo-SettingsFile
    if (-not (Test-Path $Demo.Backup)) { New-Item -ItemType Directory -Force (Split-Path $Demo.Backup) | Out-Null; Copy-Item $file $Demo.Backup }
    $text = [IO.File]::ReadAllText($file)
    $text = $text.Replace('screenMode = "BORDERLESS"', 'screenMode = "WINDOWED"').Replace('windowSize = { 1024, 768, }', 'windowSize = { 1280, 1340, }')
    [IO.File]::WriteAllText($file, $text, (New-Object Text.UTF8Encoding $false))
}

# One game process with its own live folder and the developer connection.
function Start-GameInstance([string]$LiveName) {
    $env:FEVERLINK3_LIVE_DIR = Join-Path $env:LOCALAPPDATA ('FeverLink3\' + $LiveName)
    $env:FEVERLINK3_TRANSPORT = 'tcp-loopback'
    # the single-worker script pool (docs/ARCHITECTURE.md section 7): without it two games on a many-core PC update their scripts in
    # different orders and town growth drifts. FL3_DEMO_POOLPATCH=0 turns it off to see the drift.
    $env:FEVERLINK3_POOLPATCH = if ($env:FL3_DEMO_POOLPATCH) { $env:FL3_DEMO_POOLPATCH } else { '1' }
    if ($env:FL3_DEMO_DEV) { $env:FEVERLINK3_DEV = $env:FL3_DEMO_DEV }   # '1': the developer probe (dev_gui.lua / dev_sim.lua in the live folder)
    if ($env:FL3_DEMO_HOST_RELOAD) { $env:FEVERLINK3_HOST_RELOAD = $env:FL3_DEMO_HOST_RELOAD }   # '1': the host's game reloads too
    try { Start-Process (Join-Path $Demo.Game 'TransportFever3.exe') -WorkingDirectory $Demo.Game -PassThru }
    finally { $env:FEVERLINK3_LIVE_DIR = $null; $env:FEVERLINK3_TRANSPORT = $null; $env:FEVERLINK3_HOST_RELOAD = $null; $env:FEVERLINK3_DEV = $null; $env:FEVERLINK3_POOLPATCH = $null }
}

# --- screen positions inside one game window (client pixels; both windows have the same layout)
$Pos = @{
    MenuContinue  = @(368, 839)
    MenuNewGame   = @(498, 685)
    MapDry        = @(952, 555)
    ModsTab       = @(302, 433)     # "Add Mods" tab of the new-game settings
    ActivateFirst = @(386, 671)     # first mod of the list: FeverLink3
    StartGame     = @(964, 983)     # on the new-game settings page
    LoadingStart  = @(1178, 1281)   # on the loading screen
    TutorialGo    = @(1096, 197)    # "Let's get started" of the tutorial panel
    TutorialOk    = @(630, 775)     # "Ok, I got it" of the camera dialog
    TutorialClose = @(1232, 21)     # the panel's close button
    BarButton     = @(747, 1338)    # FeverLink3 button in the game's bottom bar
}

function Demo-Click($id, $key) { Click-Win $id $Pos[$key][0] $Pos[$key][1] }

function Demo-Setup {
    Get-Process TransportFever3, feverlink3 -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 3
    Demo-Step 'windowed mode for the recording, install like a player'
    Demo-PatchSettings
    Install-Overlay
    Set-Content -Path (Join-Path $Demo.Game 'steam_appid.txt') -Value '3493540' -Encoding ascii   # direct start of a second copy
    Demo-Step 'start both games'
    $left = Start-GameInstance 'live1'
    Start-Sleep 35
    Move-Game $left.Id 0 0 1280 $Demo.OuterH
    $right = Start-GameInstance 'live2'
    Start-Sleep 35
    Move-Game $right.Id 1280 0 1280 $Demo.OuterH
    Start-Sleep 3
    Demo-Step 'left: Continue (the saved map); right: a new desert map with the mod on'
    Demo-Click $left.Id 'MenuContinue'
    Demo-Click $right.Id 'MenuNewGame'
    Start-Sleep 3
    Demo-Click $right.Id 'MapDry'
    Start-Sleep 4
    Demo-Click $right.Id 'ModsTab'
    Start-Sleep 3
    Demo-Click $right.Id 'ActivateFirst'
    Start-Sleep 2
    Demo-Click $right.Id 'StartGame'
    Start-Sleep 55
    Demo-Click $left.Id 'LoadingStart'
    Start-Sleep 55
    Demo-Click $right.Id 'LoadingStart'
    Start-Sleep 20
    foreach ($id in @($left.Id, $right.Id)) {          # the tutorial of a fresh game would sit on the picture
        Demo-Click $id 'TutorialGo'; Start-Sleep 2
        Demo-Click $id 'TutorialOk'; Start-Sleep 2
        Demo-Click $id 'TutorialClose'; Start-Sleep 2
    }
    Move-Game $left.Id 0 0 1280 $Demo.OuterH
    Move-Game $right.Id 1280 0 1280 $Demo.OuterH
    Start-Sleep 3
    Demo-Step 'both games are in their own world'
    return @{ Left = $left.Id; Right = $right.Id }
}

if ($MyInvocation.InvocationName -ne '.') {
    $ids = Demo-Setup
    $shots = Join-Path $env:TEMP 'fl3-demo'
    New-Item -ItemType Directory -Force $shots | Out-Null
    Save-Region (Join-Path $shots 'setup_done.png') 0 0 2560 1396 1280
    Write-Host ("left pid {0}, right pid {1}" -f $ids.Left, $ids.Right)
}
