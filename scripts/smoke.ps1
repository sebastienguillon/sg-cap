# Smoke test: launch sgcap, trigger the region hotkey, drag a selection with
# synthetic input, verify the overlay, the thumbnail window and the saved PNG.
param(
    [string]$Exe = "D:\dev\projects\sg-cap\target\debug\sgcap.exe",
    [string]$Out = "C:\Users\ghola\AppData\Local\Temp\claude\D--dev-projects-sg-cap\74f3f1be-a673-43ab-a8e1-8bee228689f9\scratchpad",
    [int]$X0 = 1100, [int]$Y0 = 500, [int]$X1 = 1420, [int]$Y1 = 700,
    [switch]$KeepRunning
)
$ErrorActionPreference = "Continue"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Native {
  [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, UIntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, IntPtr name);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@

function Shot([string]$name, [int]$l, [int]$t, [int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($l, $t, 0, 0, $bmp.Size)
    $g.Dispose()
    $p = Join-Path $Out $name
    $bmp.Save($p, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return $p
}

function WinRect([IntPtr]$h) {
    $r = New-Object Native+RECT
    [void][Native]::GetWindowRect($h, [ref]$r)
    return "$($r.Left),$($r.Top)-$($r.Right),$($r.Bottom) ($($r.Right-$r.Left)x$($r.Bottom-$r.Top))"
}

$log = Join-Path $env:APPDATA "SgCap\sgcap.log"
$desktop = [Environment]::GetFolderPath("Desktop")
$before = Get-ChildItem $desktop -Filter "Screenshot *.png" -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name

Get-Process sgcap -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
$env:SGCAP_DUMP_THUMB = $Out
$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1500
"process alive: " + (-not $proc.HasExited)
"main window: " + ([Native]::FindWindowW("SgCapMain", [IntPtr]::Zero) -ne [IntPtr]::Zero)
"overlay exists: " + ([Native]::FindWindowW("SgCapOverlay", [IntPtr]::Zero) -ne [IntPtr]::Zero)

$orig = New-Object Native+POINT
[void][Native]::GetCursorPos([ref]$orig)

# Hotkey Ctrl+Shift+4
[void][Native]::SetCursorPos($X0, $Y0)
Start-Sleep -Milliseconds 100
$t0 = Get-Date
[Native]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero)
[Native]::keybd_event(0x10, 0, 0, [UIntPtr]::Zero)
[Native]::keybd_event(0x34, 0, 0, [UIntPtr]::Zero)
[Native]::keybd_event(0x34, 0, 2, [UIntPtr]::Zero)
[Native]::keybd_event(0x10, 0, 2, [UIntPtr]::Zero)
[Native]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero)
# Wait for the overlay to become visible, measure latency
$ov = [Native]::FindWindowW("SgCapOverlay", [IntPtr]::Zero)
$visible = $false
for ($i = 0; $i -lt 100; $i++) { if ([Native]::IsWindowVisible($ov)) { $visible = $true; break }; Start-Sleep -Milliseconds 10 }
$lat = ((Get-Date) - $t0).TotalMilliseconds
"overlay visible: $visible after ~$([int]$lat) ms, rect " + (WinRect $ov)
"foreground is overlay: " + ([Native]::GetForegroundWindow() -eq $ov)
Start-Sleep -Milliseconds 300
"shot1: " + (Shot "t1_overlay.png" ($X0 - 200) ($Y0 - 150) 700 500)

# Drag selection
[Native]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)   # left down
Start-Sleep -Milliseconds 60
$steps = 12
for ($i = 1; $i -le $steps; $i++) {
    $x = $X0 + [int](($X1 - $X0) * $i / $steps)
    $y = $Y0 + [int](($Y1 - $Y0) * $i / $steps)
    [void][Native]::SetCursorPos($x, $y)
    Start-Sleep -Milliseconds 25
}
Start-Sleep -Milliseconds 300
"shot2: " + (Shot "t2_selection.png" ($X0 - 200) ($Y0 - 150) 700 500)
[Native]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)   # left up
$tRelease = Get-Date

# Thumbnail should appear
$thumb = [IntPtr]::Zero
for ($i = 0; $i -lt 200; $i++) { $thumb = [Native]::FindWindowW("SgCapThumb", [IntPtr]::Zero); if ($thumb -ne [IntPtr]::Zero) { break }; Start-Sleep -Milliseconds 10 }
$lat2 = ((Get-Date) - $tRelease).TotalMilliseconds
"overlay hidden: " + (-not [Native]::IsWindowVisible($ov))
"thumbnail window: " + ($thumb -ne [IntPtr]::Zero) + " after ~$([int]$lat2) ms"
Start-Sleep -Milliseconds 600
if ($thumb -ne [IntPtr]::Zero) { "thumb rect (after slide-in): " + (WinRect $thumb) + " visible=" + [Native]::IsWindowVisible($thumb) }
"shot3: " + (Shot "t3_corner.png" (2560 - 500) (1440 - 400) 500 400)

# Saved file
Start-Sleep -Milliseconds 500
$after = Get-ChildItem $desktop -Filter "Screenshot *.png" -ErrorAction SilentlyContinue | Where-Object { $before -notcontains $_.Name }
"new files: " + (($after | ForEach-Object { "$($_.Name) [$($_.Length) bytes]" }) -join "; ")
if ($after) { Copy-Item $after[0].FullName (Join-Path $Out "t4_saved.png") -Force }

# Thumbnail should vanish after ~5 s
Start-Sleep -Seconds 6
"thumbnail gone: " + ([Native]::FindWindowW("SgCapThumb", [IntPtr]::Zero) -eq [IntPtr]::Zero)

[void][Native]::SetCursorPos($orig.X, $orig.Y)
"--- log tail"
Get-Content $log -Tail 15
if (-not $KeepRunning) { Get-Process sgcap -ErrorAction SilentlyContinue | Stop-Process -Force }
