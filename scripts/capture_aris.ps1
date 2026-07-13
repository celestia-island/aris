# capture_aris.ps1 — capture the aris_browser window to a 24bpp JPEG/PNG.
#
# Usage:
#   powershell -File scripts/capture_aris.ps1 [output.png] [region]
#     region: "full" (default) | "chrome" (top 50px) | "page" (below chrome)
#
# Brings the window to the foreground, waits, captures the client area, and
# re-encodes as 24bpp RGB (no alpha) so downstream image tools accept it.
# Prints the output path and a few diagnostic pixel samples.

param(
    [string]$Out = "D:\tmp\aris_capture.png",
    [string]$Region = "full"
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class ArisWin {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L; public int T; public int R; public int B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
}
"@

$p = Get-Process aris_browser -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Error "aris_browser not running"; exit 1 }
$hWnd = $p.MainWindowHandle
if ($hWnd -eq [IntPtr]::Zero) { Write-Error "no main window"; exit 1 }

# Restore + focus, with retries (window may be minimized).
[ArisWin]::ShowWindow($hWnd, 9) | Out-Null   # SW_RESTORE
Start-Sleep -Milliseconds 200
for ($i = 0; $i -lt 5; $i++) {
    [ArisWin]::SetForegroundWindow($hWnd) | Out-Null
    Start-Sleep -Milliseconds 300
    if ([ArisWin]::SetForegroundWindow($hWnd)) { break }
}
Start-Sleep -Milliseconds 600

$c = New-Object ArisWin+RECT
[ArisWin]::GetClientRect($hWnd, [ref]$c) | Out-Null
$pt = New-Object ArisWin+POINT; $pt.X = 0; $pt.Y = 0
[ArisWin]::ClientToScreen($hWnd, [ref]$pt) | Out-Null
$cw = $c.R - $c.L; $ch = $c.B - $c.T
if ($cw -le 0 -or $ch -le 0) { Write-Error "empty client area"; exit 1 }

# Pick the sub-region.
$capY = 0; $capH = $ch
if ($Region -eq "chrome") { $capH = [Math]::Min(50, $ch) }
elseif ($Region -eq "page") { $capY = 50; $capH = $ch - 50 }

# Capture at 24bpp RGB (no alpha) for max compatibility.
$bmp = New-Object System.Drawing.Bitmap($cw, $capH, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($pt.X, $pt.Y + $capY, 0, 0, $bmp.Size)
$g.Dispose()

# Ensure dir exists.
$outDir = [System.IO.Path]::GetDirectoryName($Out)
if ($outDir -and -not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir | Out-Null }

# Save as PNG (24bpp). Re-encode to strip any DPI/extra chunks.
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()

Write-Output "$Out ($cw x $capH, region=$Region)"
