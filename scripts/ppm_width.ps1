Add-Type -AssemblyName System.Drawing
$path = $env:LOCALAPPDATA + '\Temp\wt.ppm'
$b = [System.IO.File]::ReadAllBytes($path)
# Parse PPM header
$nl = 0; $p = 0
$lines = @()
for ($i=0; $i -lt $b.Length; $i++) { if ($b[$i] -eq 10) { $lines += $i; if ($lines.Count -ge 3) { break } } }
$hdr = [System.Text.Encoding]::ASCII.GetString($b[0..($lines[2]-1)])
Write-Output ("header: " + $hdr)
$dims = ([System.Text.Encoding]::ASCII.GetString($b[($lines[0]+1)..($lines[1]-1)]) -split ' ')
$w = [int]$dims[0]; $h = [int]$dims[1]
Write-Output ("dims: " + $w + "x" + $h)
$dstart = $lines[2] + 1
# Row 0: red pixels
$red = 0
for ($x=0; $x -lt $w; $x++) {
  $off = $dstart + $x*3
  $r=$b[$off]; $g=$b[$off+1]; $bb=$b[$off+2]
  if ($r -gt 200 -and $g -lt 50 -and $bb -lt 50) { $red++ }
}
Write-Output ("row0 red pixels: " + $red + " of " + $w)
# Last row (h-1) red pixels
$rowoff = $dstart + ($h-1)*$w*3
$redlast = 0
for ($x=0; $x -lt $w; $x++) {
  $off = $rowoff + $x*3
  $r=$b[$off]; $g=$b[$off+1]; $bb=$b[$off+2]
  if ($r -gt 200 -and $g -lt 50 -and $bb -lt 50) { $redlast++ }
}
Write-Output ("last row red pixels: " + $redlast + " of " + $w)
