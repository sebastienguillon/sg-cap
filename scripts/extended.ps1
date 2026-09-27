# Extended checks: screen hotkey, thumbnail stacking, hover pause, context menu,
# settings window, single instance.
param(
    [string]$Exe = "D:\dev\projects\sg-cap\target\debug\sgcap.exe",
    [string]$Out = "C:\Users\ghola\AppData\Local\Temp\claude\D--dev-projects-sg-cap\74f3f1be-a673-43ab-a8e1-8bee228689f9\scratchpad"
)
$ErrorActionPreference = "Continue"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class NE {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, UIntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, IntPtr name);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  public static List<IntPtr> ByClass(string cls) { var r = new List<IntPtr>(); EnumWindows((h, l) => { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); if (sb.ToString() == cls && IsWindowVisible(h)) r.Add(h); return true; }, IntPtr.Zero); return r; }
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  public static string ClassAt(int x, int y) { var p = new POINT { X = x, Y = y }; var h = WindowFromPoint(p); var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return h + " " + sb; }
}
"@
function Shot([string]$name, [int]$l, [int]$t, [int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($l, $t, 0, 0, $bmp.Size); $g.Dispose()
    $p = Join-Path $Out $name; $bmp.Save($p, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose(); return $p
}
function GetRect([IntPtr]$h) { $r = New-Object NE+RECT; [void][NE]::GetWindowRect($h, [ref]$r); return $r }
function RectStr([IntPtr]$h) { $r = GetRect $h; "$($r.Left),$($r.Top)-$($r.Right),$($r.Bottom)" }
function Hotkey([byte]$vk) {
    [NE]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero); [NE]::keybd_event(0x10, 0, 0, [UIntPtr]::Zero)
    [NE]::keybd_event($vk, 0, 0, [UIntPtr]::Zero); [NE]::keybd_event($vk, 0, 2, [UIntPtr]::Zero)
    [NE]::keybd_event(0x10, 0, 2, [UIntPtr]::Zero); [NE]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero)
}
function DragRegion([int]$x0, [int]$y0, [int]$x1, [int]$y1) {
    [void][NE]::SetCursorPos($x0, $y0); Start-Sleep -Milliseconds 80
    Hotkey 0x34; Start-Sleep -Milliseconds 350
    [NE]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 50
    for ($i = 1; $i -le 8; $i++) { [void][NE]::SetCursorPos($x0 + [int](($x1 - $x0) * $i / 8), $y0 + [int](($y1 - $y0) * $i / 8)); Start-Sleep -Milliseconds 20 }
    [NE]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
}
function WaitThumbs([int]$n, [int]$ms) { for ($i = 0; $i -lt ($ms / 20); $i++) { if (([NE]::ByClass("SgCapThumb")).Count -ge $n) { return $true }; Start-Sleep -Milliseconds 20 }; return $false }

$desktop = [Environment]::GetFolderPath("Desktop")
$before = Get-ChildItem $desktop -Filter "Screenshot *.png" | Select-Object -ExpandProperty Name
# Thumbnail delay from settings.json (default 12 s)
$delay = 12
try { $cfg = Get-Content (Join-Path $env:APPDATA "SgCap\settings.json") -Raw | ConvertFrom-Json; if ($cfg.thumbnail_seconds) { $delay = [int]$cfg.thumbnail_seconds } } catch {}
"thumbnail delay: $delay s"
Get-Process sgcap -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
Remove-Item Env:SGCAP_DUMP_THUMB -ErrorAction SilentlyContinue
$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1800
$orig = New-Object NE+POINT; [void][NE]::GetCursorPos([ref]$orig)

"--- 1. screen hotkey (Ctrl+Shift+3)"
[void][NE]::SetCursorPos(1200, 600); Start-Sleep -Milliseconds 80
Hotkey 0x33
"thumb appeared: " + (WaitThumbs 1 3000)
Start-Sleep -Milliseconds 900
$files = Get-ChildItem $desktop -Filter "Screenshot *.png" | Where-Object { $before -notcontains $_.Name }
foreach ($f in $files) { $img = [System.Drawing.Image]::FromFile($f.FullName); "file: $($f.Name) $($img.Width)x$($img.Height) $($f.Length) bytes"; $img.Dispose() }

"--- 2. second capture while first thumbnail is up (stacking)"
DragRegion 900 300 1300 520
"two thumbs: " + (WaitThumbs 2 3000)
Start-Sleep -Milliseconds 700
[NE]::ByClass("SgCapThumb") | ForEach-Object { "thumb " + (RectStr $_) }
"shot: " + (Shot "e1_stack.png" (2560 - 520) (1440 - 560) 520 560)

"--- 3. hover keeps the newest thumbnail alive"
$thumbs = [NE]::ByClass("SgCapThumb")
$newest = $thumbs | Sort-Object { (GetRect $_).Top } -Descending | Select-Object -First 1
$r = GetRect $newest; $cx = [int](($r.Left + $r.Right) / 2); $cy = [int](($r.Top + $r.Bottom) / 2)
[void][NE]::SetCursorPos($cx, $cy); Start-Sleep -Milliseconds 100; [NE]::mouse_event(0x0001, 1, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Seconds ($delay + 2)
"hovered thumb still alive after $($delay + 2) s: " + ([NE]::IsWindowVisible($newest))
"older thumb gone meanwhile: " + (([NE]::ByClass("SgCapThumb")).Count -eq 1)
[void][NE]::SetCursorPos(600, 600); [NE]::mouse_event(0x0001, 1, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds (($delay + 1.5) * 1000)
"gone $($delay + 1.5) s after leaving: " + (([NE]::ByClass("SgCapThumb")).Count -eq 0)

"--- 4. context menu"
DragRegion 900 300 1200 500
"thumb: " + (WaitThumbs 1 3000)
Start-Sleep -Milliseconds 600
$t = ([NE]::ByClass("SgCapThumb"))[0]
$r = GetRect $t; $cx = [int](($r.Left + $r.Right) / 2); $cy = [int](($r.Top + $r.Bottom) / 2)
[void][NE]::SetCursorPos($cx, $cy); Start-Sleep -Milliseconds 120; [NE]::mouse_event(0x0001, 1, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 100
"window under cursor before right-click: " + [NE]::ClassAt($cx + 1, $cy)
[NE]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero); [NE]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 500
"shot: " + (Shot "e2_menu.png" ($cx - 300) ($cy - 250) 500 400)
[NE]::keybd_event(0x1B, 0, 0, [UIntPtr]::Zero); [NE]::keybd_event(0x1B, 0, 2, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 300
[void][NE]::SetCursorPos(600, 600); [NE]::mouse_event(0x0001, 1, 0, 0, [UIntPtr]::Zero)
"thumb alive after menu closed: " + ([NE]::IsWindowVisible($t))
Start-Sleep -Milliseconds (($delay + 1.5) * 1000)
"thumb gone later: " + (([NE]::ByClass("SgCapThumb")).Count -eq 0)

"--- 5. settings window via tray command"
$main = [NE]::FindWindowW("SgCapMain", [IntPtr]::Zero)
[void][NE]::PostMessageW($main, 0x111, [IntPtr]203, [IntPtr]::Zero)
Start-Sleep -Milliseconds 900
$s = [NE]::FindWindowW("SgCapSettings", [IntPtr]::Zero)
"settings window: " + ($s -ne [IntPtr]::Zero) + " " + (RectStr $s)
$r = GetRect $s
"shot: " + (Shot "e3_settings.png" ($r.Left - 10) ($r.Top - 10) ($r.Right - $r.Left + 20) ($r.Bottom - $r.Top + 20))
[void][NE]::PostMessageW($s, 0x111, [IntPtr]2, [IntPtr]::Zero)
Start-Sleep -Milliseconds 500
"settings closed: " + ([NE]::FindWindowW("SgCapSettings", [IntPtr]::Zero) -eq [IntPtr]::Zero)

"--- 6. second instance opens settings in the first"
$p2 = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1200
"second instance exited: " + $p2.HasExited
"sgcap processes: " + (Get-Process sgcap -ErrorAction SilentlyContinue).Count
$s = [NE]::FindWindowW("SgCapSettings", [IntPtr]::Zero)
"settings opened by broadcast: " + ($s -ne [IntPtr]::Zero)
if ($s -ne [IntPtr]::Zero) { [void][NE]::PostMessageW($s, 0x111, [IntPtr]2, [IntPtr]::Zero) }
Start-Sleep -Milliseconds 400

[void][NE]::SetCursorPos($orig.X, $orig.Y)
"--- log tail"
Get-Content (Join-Path $env:APPDATA "SgCap\sgcap.log") -Tail 8
Get-Process sgcap -ErrorAction SilentlyContinue | Stop-Process -Force
"--- cleanup test screenshots from Desktop"
Get-ChildItem $desktop -Filter "Screenshot *.png" | Where-Object { $before -notcontains $_.Name } | ForEach-Object { "removing $($_.Name)"; Remove-Item $_.FullName }
