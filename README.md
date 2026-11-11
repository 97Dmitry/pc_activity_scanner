# PC Activity Scanner

Local desktop process and resource monitor in **Rust**. CPU, GPU, RAM, network throughput per process where supported; searchable/sortable table, grouping, filters, session totals, pause, CSV export, history chart and optional minimize-to-tray.

[Specification / acceptance criteria](docs/SPEC.md) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Release artifacts](https://github.com/97Dmitry/pc_activity_scanner/releases)

## Quick start

### Linux (Kubuntu / Ubuntu)

```bash
sudo apt update
sudo apt install -y libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libwayland-dev libssl-dev pkg-config
# Optional per-process network metrics:
sudo apt install -y nethogs policykit-1
cargo run --release # CPU/RAM and unprivileged metrics
bash scripts/install-linux.sh # root-owned installation required for NetHogs
pc-activity-scanner
```

Rust **1.95+** required. Detailed NetHogs tracking requires a **root-owned executable in `/usr/local/bin`**. Development builds in user-writable directories are deliberately rejected as privileged helpers to prevent binary replacement between authorization and execution. CPU/RAM/global networking/GPU work without installation. Enable per-process network measurements in **Settings → Network**. A PolicyKit authentication dialog is expected. Running the entire UI as root is **not** recommended. On KDE Plasma Wayland, the desktop environment must expose a system tray (StatusNotifierItem) for close-to-tray.

### Windows 11

Install latest stable Rust/MSVC toolchain (Visual Studio C++ build tools), then:

```powershell
cargo run --release
```

CPU, RAM and global network counters are available; detailed GPU and network measurements **per PID are not implemented on Windows yet**, so those columns show `—`. Do not treat global traffic as attributable to individual processes.

## Features

- Sortable, resizable process table with per-PID CPU (percentage of all cores), GPU (where exposed), RAM, GPU VRAM, RX/TX bytes/s, RX/TX session bytes.
- Search by process, PID or executable path. Filters for active/network, CPU/RAM thresholds; group by process name.
- Current total CPU/RAM and overall network RX/TX, 120-point CPU/RAM usage graph.
- CSV export of *currently displayed* rows (UTF-8 with BOM, semicolon separator, CSV quoting, formula-injection protection and collision-free filenames).
- Configurable 1/2/5s polling; in-memory pause and explicit exit control.
- Optional close-to-tray with Restore/Quit menu and automatic fallback if tray unavailable.
- Settings stored locally under per-user OS config directory. No telemetry, no internet access required for runtime.

## Metrics accuracy

- CPU per process is sampled (not a lifetime average), normalized to all logical cores.
- Per-PID network traffic uses NetHogs in trace mode for Linux; its bytes/s are converted to B/s. Session totals integrate one unique frame once, so they are estimates. Some processes and short-lived flows can be missing.
- Global network counters and sum of per-process counters will not generally match because monitoring interfaces, privilege restrictions and protocols differ.
- Linux GPU uses DRM fdinfo, optional NVIDIA pmon, and may not work on all drivers; missing value is `—`. A GPU % is per-process sampled usage, not the sum of all GPU engines.
- If a metric is unsupported, disabled, or permission-denied, the UI displays a status and `—` instead of 0.
- The app doesn't persist raw process history between restarts. It does not collect visited sites or application content.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

Specs/tests live in [`docs/SPEC.md`](docs/SPEC.md) and the [manual QA checklist](docs/QA_CHECKLIST.md) and Rust `#[cfg(test)]` modules. GitHub Actions executes checks and publishes **pre-releases** with Windows and Linux executables on every push to `main` after both builds pass. These builds are not code-signed. Releases tagged `snapshot-<full SHA>` are immutable snapshots, not stable releases.

## Known limitations

See the platform support table in [SPEC.md](docs/SPEC.md). In particular: no native Windows per-PID network/GPU metrics yet, GPU % is driver-specific, display-server tray integration must be smoke-tested, and elevated NetHogs access may require Polkit configuration. This is an **initial release candidate**, not a certified production deployment.

## License

MIT.
