# Application artwork

Original vector artwork for PC Activity Scanner, distributed under the project
MIT license. No external images or fonts are used. The desktop icon uses a
monitor with a resource trace; the tray version has fewer details and a dark
outline to remain visible on both light and dark panels.

`app.svg` and `tray.svg` are the editable sources. Regenerate PNG, ICO and raw
RGBA assets with `bash scripts/generate-icons.sh` (ImageMagick 7). Generated
files are tracked; building and running require no graphics tools.

`app.ico` includes 16/24/32/48/64/128/256 sizes and is embedded into the Windows
EXE by `build.rs`. Raw RGBA files are compiled into the window/tray icon without
runtime decoding. Linux installation registers SVG and 256px PNG in hicolor.
