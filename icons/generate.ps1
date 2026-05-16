Add-Type -AssemblyName System.Drawing

$src = Join-Path $PSScriptRoot "mHD_256.png"
$img = [System.Drawing.Image]::FromFile($src)

foreach ($size in @(16, 32)) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.DrawImage($img, 0, 0, $size, $size)

    $dest = Join-Path $PSScriptRoot "mHD_$size.png"
    $bmp.Save($dest, [System.Drawing.Imaging.ImageFormat]::Png)

    $g.Dispose()
    $bmp.Dispose()
    Write-Host "Created: $dest"
}

$img.Dispose()
