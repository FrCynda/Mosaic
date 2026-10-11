# Builds the release installer: dist\MosaicSetup.exe
#   cargo build --release  ->  stamp the named exes into dist\  ->  Inno Setup
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE) { throw 'cargo build failed' }

# the installer stamps the final named exes itself (with the icon presets the user picks), from this one base exe
New-Item -ItemType Directory -Force dist\prev | Out-Null
Remove-Item dist\prev\*.bmp -ErrorAction SilentlyContinue
Copy-Item .\target\release\mosaic-wrapper.exe dist\mosaic-base.exe -Force
Get-ChildItem icons\presets\*-48.png | ForEach-Object { & magick $_.FullName -background '#f0f0f0' -flatten "BMP3:dist\prev\$($_.Name -replace '-48\.png$','.bmp')" }
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (winget install JRSoftware.InnoSetup)' }
& $iscc installer\mosaic.iss
if ($LASTEXITCODE) { throw 'installer build failed' }
Get-Item dist\MosaicSetup.exe | Select-Object Name, @{ n = 'MB'; e = { [math]::Round($_.Length / 1MB, 1) } }

