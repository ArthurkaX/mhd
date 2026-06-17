param(
    [string]$Version = "0.4.0"
)

$ErrorActionPreference = "Stop"

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$dist = Join-Path $root "dist"
$targetDir = Join-Path $dist "cargo-target"
$stage = Join-Path $dist "mhd-v$Version-windows-x64"
$zip = "$stage.zip"
$checksum = "$zip.sha256"

Push-Location $root
try {
    cargo build --release --no-default-features --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    if (Test-Path -LiteralPath $stage) {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
    if (Test-Path -LiteralPath $zip) {
        Remove-Item -LiteralPath $zip -Force
    }
    if (Test-Path -LiteralPath $checksum) {
        Remove-Item -LiteralPath $checksum -Force
    }

    New-Item -ItemType Directory -Path $stage | Out-Null

    Copy-Item -LiteralPath (Join-Path $targetDir "release\mhd.exe") -Destination $stage
    Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination $stage
    Copy-Item -LiteralPath (Join-Path $root "INSTALL.md") -Destination $stage
    Copy-Item -LiteralPath (Join-Path $root "README.md") -Destination $stage
    Copy-Item -LiteralPath (Join-Path $root "claude-mhd.bat") -Destination $stage

    $packageFiles = Get-ChildItem -LiteralPath $stage
    Compress-Archive -Path $packageFiles.FullName -DestinationPath $zip -CompressionLevel Optimal

    $hash = Get-FileHash -LiteralPath $zip -Algorithm SHA256
    "$($hash.Hash)  $(Split-Path $zip -Leaf)" | Set-Content -LiteralPath $checksum -Encoding ascii

    Write-Host "Release archive: $zip"
    Write-Host "SHA256: $($hash.Hash)"
}
finally {
    Pop-Location
}
