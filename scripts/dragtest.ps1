# Drag-and-drop test: a WinForms drop target records the formats it receives
# when the sgcap thumbnail is dragged onto it with synthetic mouse input.
param(
    [string]$Exe = "D:\dev\projects\sg-cap\target\debug\sgcap.exe",
    [int]$X0 = 1100, [int]$Y0 = 500, [int]$X1 = 1420, [int]$Y1 = 700
)
$ErrorActionPreference = "Continue"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class NativeD {
  [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, UIntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, IntPtr name);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@

function Pump([int]$ms) { $end = (Get-Date).AddMilliseconds($ms); while ((Get-Date) -lt $end) { [System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 5 } }

$script:events = New-Object System.Collections.ArrayList
$form = New-Object System.Windows.Forms.Form
$form.Text = "SgCap drop target"
$form.StartPosition = "Manual"
$form.Location = New-Object System.Drawing.Point(200, 200)
$form.Size = New-Object System.Drawing.Size(500, 400)
$form.TopMost = $true
$form.AllowDrop = $true
$form.BackColor = [System.Drawing.Color]::LightYellow
$form.Add_DragEnter({ param($s, $e)
    $e.Effect = [System.Windows.Forms.DragDropEffects]::Copy
    [void]$script:events.Add("DragEnter formats: " + (($e.Data.GetFormats() | ForEach-Object { $_ }) -join ", "))
})
$form.Add_DragDrop({ param($s, $e)
    $files = $e.Data.GetData([System.Windows.Forms.DataFormats]::FileDrop)
    [void]$script:events.Add("DragDrop files: " + ($files -join "; "))
    $hasDib = $e.Data.GetDataPresent([System.Windows.Forms.DataFormats]::Dib)
    $hasPng = $e.Data.GetDataPresent("PNG")
    [void]$script:events.Add("DragDrop DIB present: $hasDib, PNG present: $hasPng")
    try { $img = $e.Data.GetData([System.Windows.Forms.DataFormats]::Bitmap); if ($img) { [void]$script:events.Add("Bitmap via DIB: $($img.Width)x$($img.Height)") } } catch { [void]$script:events.Add("bitmap read failed: $_") }
})
$form.Show()
Pump 300

Get-Process sgcap -ErrorAction SilentlyContinue | Stop-Process -Force
Pump 300
$proc = Start-Process -FilePath $Exe -PassThru
Pump 1500

$orig = New-Object NativeD+POINT
[void][NativeD]::GetCursorPos([ref]$orig)

[void][NativeD]::SetCursorPos($X0, $Y0)
Pump 100
[NativeD]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero)
[NativeD]::keybd_event(0x10, 0, 0, [UIntPtr]::Zero)
[NativeD]::keybd_event(0x34, 0, 0, [UIntPtr]::Zero)
[NativeD]::keybd_event(0x34, 0, 2, [UIntPtr]::Zero)
[NativeD]::keybd_event(0x10, 0, 2, [UIntPtr]::Zero)
[NativeD]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero)
Pump 400
$ov = [NativeD]::FindWindowW("SgCapOverlay", [IntPtr]::Zero)
"overlay visible: " + [NativeD]::IsWindowVisible($ov)
[NativeD]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
Pump 60
for ($i = 1; $i -le 10; $i++) { [void][NativeD]::SetCursorPos($X0 + [int](($X1 - $X0) * $i / 10), $Y0 + [int](($Y1 - $Y0) * $i / 10)); Pump 25 }
[NativeD]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)

$thumb = [IntPtr]::Zero
for ($i = 0; $i -lt 300; $i++) { $thumb = [NativeD]::FindWindowW("SgCapThumb", [IntPtr]::Zero); if ($thumb -ne [IntPtr]::Zero) { break }; Pump 10 }
"thumbnail window: " + ($thumb -ne [IntPtr]::Zero)
Pump 700
$r = New-Object NativeD+RECT
[void][NativeD]::GetWindowRect($thumb, [ref]$r)
"thumb rect: $($r.Left),$($r.Top)-$($r.Right),$($r.Bottom)"
$cx = [int](($r.Left + $r.Right) / 2); $cy = [int](($r.Top + $r.Bottom) / 2)

# Drag the thumbnail onto the form
[void][NativeD]::SetCursorPos($cx, $cy)
Pump 150
[NativeD]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
Pump 100
$tx = 450; $ty = 400
for ($i = 1; $i -le 30; $i++) {
    [void][NativeD]::SetCursorPos($cx + [int](($tx - $cx) * $i / 30), $cy + [int](($ty - $cy) * $i / 30))
    Pump 30
}
Pump 300
[NativeD]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
Pump 1500
"thumbnail still present: " + ([NativeD]::FindWindowW("SgCapThumb", [IntPtr]::Zero) -ne [IntPtr]::Zero)
"--- drop target events"
$script:events | ForEach-Object { $_ }
"--- log tail"
Get-Content (Join-Path $env:APPDATA "SgCap\sgcap.log") -Tail 6
[void][NativeD]::SetCursorPos($orig.X, $orig.Y)
$form.Close()
Get-Process sgcap -ErrorAction SilentlyContinue | Stop-Process -Force
