Add-Type -AssemblyName System.Drawing
$bmp = [System.Drawing.Image]::FromFile('D:\tmp\oc.png')
Write-Output ("image: " + $bmp.Width + "x" + $bmp.Height)
# ASCII map of the icon+address area
for ($y=0; $y -lt 44; $y+=2) {
  $row = ""
  for ($x=0; $x -lt 380; $x+=2) {
    $px = $bmp.GetPixel($x,$y)
    if ($px.R -gt 110 -and $px.G -gt 110 -and $px.B -gt 110) { $row += "#" }
    elseif ($px.R -gt 60 -and $px.G -gt 60 -and $px.B -gt 60) { $row += "." }
    else { $row += " " }
  }
  Write-Output $row
}
$bmp.Dispose()
