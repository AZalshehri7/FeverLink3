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
