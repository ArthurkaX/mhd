$pngPath = Join-Path $PSScriptRoot "mHD_256.png"
$icoPath = Join-Path $PSScriptRoot "mhd.ico"

$pngBytes = [System.IO.File]::ReadAllBytes($pngPath)
$pngSize = $pngBytes.Length

$icoHeader = [byte[]]@(
    0, 0, # Reserved
    1, 0, # Type (Icon)
    1, 0  # Count
)

$icoEntry = [byte[]]@(
    0, 0, # Width, Height (0 means 256)
    0,    # Color count
    0,    # Reserved
    1, 0, # Planes
    32, 0,# Bit count
    ($pngSize -band 0xff),
    (($pngSize -shr 8) -band 0xff),
    (($pngSize -shr 16) -band 0xff),
    (($pngSize -shr 24) -band 0xff),
    22, 0, 0, 0 # Offset (Header(6) + Entry(16) = 22)
)

$stream = [System.IO.File]::Create($icoPath)
$stream.Write($icoHeader, 0, $icoHeader.Length)
$stream.Write($icoEntry, 0, $icoEntry.Length)
$stream.Write($pngBytes, 0, $pngBytes.Length)
$stream.Close()

Write-Host "Created: $icoPath"
