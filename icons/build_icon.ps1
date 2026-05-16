Add-Type -AssemblyName System.Drawing

$dest = Join-Path $PSScriptRoot "mhd.ico"

$icon = New-Object System.Drawing.Icon((Join-Path $PSScriptRoot "mHD_256.png"), 256, 256)

$stream = [System.IO.File]::Create($dest)
$icon.Save($stream)
$stream.Close()
$icon.Dispose()

Write-Host "Created: $dest"
