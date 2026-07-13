Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Drawing
$bmp = [System.Drawing.Image]::FromFile('D:\tmp\oc.png')
# Zoom into the reload icon region (x 70-104, y 4-40). Print 1px-per-char.
for ($y=2; $y -lt 42; $y++) {
  $row = ""
  for ($x=68; $x -lt 104; $x++) {
    $px = $bmp.GetPixel($x,$y)
    if ($px.R -gt 150 -and $px.G -gt 150 -and $px.B -gt 150) { $row += "#" }
    elseif ($px.R -gt 80 -and $px.G -gt 80 -and $px.B -gt 80) { $row += "+" }
    elseif ($px.R -gt 50 -and $px.G -gt 50 -and $px.B -gt 50) { $row += "." }
    else { $row += " " }
  }
  Write-Output $row
}
$bmp.Dispose()
