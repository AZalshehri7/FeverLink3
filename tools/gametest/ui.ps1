# Helpers for driving the game window from scripts: screenshots and mouse clicks in 1280x720 coordinates
# (the screen is 2560x1440; every coordinate is doubled). Dot-source this file.
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -Name Win -Namespace Fl3 -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(int f, int dx, int dy, int d, int e);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
'@

function Focus-Game([int]$ProcId) {
    $p = Get-Process -Id $ProcId -ErrorAction Stop
    [Windows.Forms.SendKeys]::SendWait('%')   # lets SetForegroundWindow through the foreground lock
    [Fl3.Win]::ShowWindow($p.MainWindowHandle, 9) | Out-Null
    [Fl3.Win]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
    Start-Sleep -Milliseconds 400
}

function Click-Game([int]$ProcId, [int]$X, [int]$Y) {
    Focus-Game $ProcId
    [Fl3.Win]::SetCursorPos($X * 2, $Y * 2) | Out-Null
    Start-Sleep -Milliseconds 300
    [Fl3.Win]::mouse_event(2, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 80
    [Fl3.Win]::mouse_event(4, 0, 0, 0, 0)
}

function Save-Screenshot([string]$Path) {
    $b = [Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object Drawing.Bitmap $b.Width, $b.Height
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($b.Location, [Drawing.Point]::Empty, $b.Size)
    $small = New-Object Drawing.Bitmap 1280, ([int](1280 * $b.Height / $b.Width))
    [Drawing.Graphics]::FromImage($small).DrawImage($bmp, 0, 0, $small.Width, $small.Height)
    $small.Save($Path)
}

# the screen positions that matter (1280x720 coordinates)
$Fl3 = @{
    MenuContinue = @(370, 505)   # main menu: continue the last save
    MenuLoadGame = @(370, 610)
    StartGame    = @(1192, 654)  # the button on the loading screen
    BarButton    = @(755, 712)   # FeverLink3 button in the bottom bar
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace Fl3 {
    public static class Geo {
        [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
        [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
        [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
        [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
        [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    }
}
'@

# Places a game window (outer size, screen pixels).
function Move-Game([int]$ProcId, [int]$X, [int]$Y, [int]$W, [int]$H) {
    $p = Get-Process -Id $ProcId -ErrorAction Stop
    [Fl3.Geo]::SetWindowPos($p.MainWindowHandle, [IntPtr]::Zero, $X, $Y, $W, $H, 0x0044) | Out-Null   # no z-order change, show
}

# The client area of a game window: where its picture starts on the screen and how big it is.
function Get-ClientArea([int]$ProcId) {
    $p = Get-Process -Id $ProcId -ErrorAction Stop
    $pt = New-Object Fl3.Geo+POINT
    [Fl3.Geo]::ClientToScreen($p.MainWindowHandle, [ref]$pt) | Out-Null
    $rc = New-Object Fl3.Geo+RECT
    [Fl3.Geo]::GetClientRect($p.MainWindowHandle, [ref]$rc) | Out-Null
    @{ X = $pt.X; Y = $pt.Y; W = $rc.Right; H = $rc.Bottom }
}

# A click at (X, Y) in the picture of that window (client pixels, not scaled).
function Click-Win([int]$ProcId, [int]$X, [int]$Y) {
    Focus-Game $ProcId
    $a = Get-ClientArea $ProcId
    [Fl3.Win]::SetCursorPos($a.X + $X, $a.Y + $Y) | Out-Null
    Start-Sleep -Milliseconds 250
    [Fl3.Win]::mouse_event(2, 0, 0, 0, 0)
    Start-Sleep -Milliseconds 80
    [Fl3.Win]::mouse_event(4, 0, 0, 0, 0)
}

# One frame of a screen region (screen pixels), scaled to the given width, saved as PNG.
function Save-Region([string]$Path, [int]$X, [int]$Y, [int]$W, [int]$H, [int]$OutWidth) {
    $bmp = New-Object Drawing.Bitmap $W, $H
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($X, $Y, 0, 0, (New-Object Drawing.Size $W, $H))
    $outH = [int]($H * $OutWidth / $W)
    $small = New-Object Drawing.Bitmap $OutWidth, $outH
    $gs = [Drawing.Graphics]::FromImage($small)
    $gs.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $gs.DrawImage($bmp, 0, 0, $OutWidth, $outH)
    $small.Save($Path)
    $g.Dispose(); $gs.Dispose(); $bmp.Dispose(); $small.Dispose()
}

# Installs the build the way a player does it: winhttp.dll next to the game program and the mod in the game's own
# mods\release folder (checked: the game loads it from there). The copy the old installer put in the user-data
# folder is removed, because a second mod with the same id would be ambiguous.
function Install-Overlay {
    $steam = Join-Path ${env:ProgramFiles(x86)} 'Steam'
    $game = Join-Path $steam 'steamapps\common\Transport Fever 3'
    $repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
    Copy-Item (Join-Path $repo 'target\release\feverlink3_native.dll') (Join-Path $game 'winhttp.dll') -Force
    robocopy (Join-Path $repo 'mod\feverlink3_1') (Join-Path $game 'mods\release\feverlink3_1') /MIR /NFL /NDL /NJH /NJS | Out-Null
    foreach ($dir in Get-ChildItem (Join-Path $steam 'userdata') -Directory -ErrorAction SilentlyContinue) {
        $old = Join-Path $dir.FullName '3493540\local\mods\feverlink3_1'
        if (Test-Path $old) { [IO.Directory]::Delete($old, $true) }
    }
}
