Add-Type -AssemblyName System.Drawing
$bmp = [System.Drawing.Image]::FromFile('D:\tmp\menu.png')
# Menu is at x=60, y=60. Items each 26px. Check the menu region for text.
# ASCII map of menu area
for ($y=58; $y -lt 210; $y+=2) {
  $row = ""
  for ($x=58; $x -lt 225; $x+=2) {
    $px = $bmp.GetPixel($x,$y)
    if ($px.R -gt 150 -and $px.G -gt 150 -and $px.B -gt 150) { $row += "#" }
    elseif ($px.R -gt 80 -and $px.G -gt 80 -and $px.B -gt 80) { $row += "." }
    elseif ($px.B -gt 130 -and $px.R -gt 60 -and $px.R -lt 130) { $row += "+" }
    else { $row += " " }
  }
  Write-Output $row
}
$bmp.Dispose()
