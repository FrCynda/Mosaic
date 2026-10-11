# Renders icons/presets-src/*.svg into icons/presets/<name>-<size>.png (the sizes native.rs's icon_set! uses).
# Needs ImageMagick (magick). Run after adding or changing an SVG; the PNGs are committed so cargo needs nothing.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$out = Join-Path $root 'icons\presets'
New-Item -ItemType Directory -Force $out | Out-Null
Remove-Item "$out\*.png" -ErrorAction SilentlyContinue
foreach ($svg in Get-ChildItem (Join-Path $root 'icons\presets-src\*.svg')) {
    # render the vector once, big; each size is then a cheap resize of that
    $big = Join-Path $env:TEMP "$($svg.BaseName)-big.png"
    & magick -background none -density 192 $svg.FullName -resize 512x512 $big
    if ($LASTEXITCODE) { throw "magick failed on $($svg.Name)" }
    foreach ($s in 16, 20, 24, 32, 40, 48, 64, 256) {
        # 8-bit palette with alpha: the gradients stay clean and the file is ~4x smaller than truecolor
        & magick $big -resize "${s}x${s}" -colors 255 "PNG8:$out\$($svg.BaseName)-$s.png"
        if ($LASTEXITCODE) { throw "magick failed on $($svg.Name)" }
    }
    Remove-Item $big
}
'{0} files, {1:N1} MB' -f (Get-ChildItem $out).Count, ((Get-ChildItem $out | Measure-Object Length -Sum).Sum / 1MB)
