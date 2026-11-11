#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release --locked
sudo install -o root -g root -m 0755 target/release/pc_activity_scanner /usr/local/bin/pc-activity-scanner
sudo install -o root -g root -m 0644 assets/pc-activity-scanner.desktop /usr/local/share/applications/pc-activity-scanner.desktop
printf 'Installed /usr/local/bin/pc-activity-scanner. Start without sudo.\n'
