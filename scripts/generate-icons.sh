#!/usr/bin/env bash
# Developer tool: requires ImageMagick 7; not needed to build or run the app.
set -euo pipefail
cd "$(dirname "$0")/.."
magick -background none -density 384 assets/icons/app.svg -resize 256x256 assets/icons/app.png
magick assets/icons/app.png -define icon:auto-resize=256,128,64,48,32,24,16 assets/icons/app.ico
magick assets/icons/app.png -resize 128x128 -depth 8 RGBA:assets/icons/app.rgba
magick -background none -density 384 assets/icons/tray.svg -resize 32x32 assets/icons/tray.png
magick assets/icons/tray.png -depth 8 RGBA:assets/icons/tray.rgba
