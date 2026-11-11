#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release --locked
sudo install -D -o root -g root -m 0755 target/release/pc_activity_scanner /usr/local/bin/pc-activity-scanner
sudo install -D -o root -g root -m 0644 assets/pc-activity-scanner.desktop /usr/local/share/applications/pc-activity-scanner.desktop
sudo install -D -o root -g root -m 0644 assets/icons/app.svg /usr/local/share/icons/hicolor/scalable/apps/pc-activity-scanner.svg
sudo install -D -o root -g root -m 0644 assets/icons/app.png /usr/local/share/icons/hicolor/256x256/apps/pc-activity-scanner.png
printf 'Installed /usr/local/bin/pc-activity-scanner. Start without sudo.\n'
