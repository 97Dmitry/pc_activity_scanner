# PC Activity Scanner — product specification (SDD)

Status: initial desktop release candidate. Version: 0.1.0.

## Objective

A local desktop utility, written in Rust, that attributes resource consumption to running processes and allows the user to find expensive applications quickly. No cloud accounts, background uploads, or analytics.

## Supported platforms and data contracts

| Signal | Linux desktop | Windows desktop |
|---|---|---|
| Total CPU/RAM | `sysinfo` | `sysinfo` |
| Per-process CPU/RAM | `sysinfo` | `sysinfo` |
| Total network RX/TX rate | `sysinfo::Networks` | `sysinfo::Networks` |
| Per-process RX/TX rate | NetHogs trace; opt-in, `pkexec`/root | **Unavailable**; show `—` |
| Per-process bytes since started | Integrate each NetHogs frame once (estimate) | **Unavailable**; show `—` |
| Per-process GPU use | DRM fdinfo, NVIDIA pmon fallback (driver dependent) | **Unavailable**; show `—` |
| GPU dedicated VRAM | DRM fdinfo, NVIDIA pmon if exposed | **Unavailable**; show `—` |

Linux CPU percentages are normalized across logical cores (100% = all logical CPU capacity). Network rate units are bytes/s, total bytes are accumulated **in the present app session**, not lifetime totals. The network backend may not attribute traffic to some processes. Unknown metrics must be represented as unknown, never as zero. GPU percentage reflects available vendor signals and is not directly comparable across drivers.

## User scenarios and acceptance criteria

1. **Live overview.** Open the app and see total CPU, used/total memory, total RX/TX rate and a sorted process table. The UI must remain responsive while sampling runs in worker threads.
2. **Explore usage.** Search by executable name, path, or PID. Filter active apps, apps consuming network traffic, minimum CPU and RAM. Group same-name processes. Sort by clicking any metric header and flip direction by clicking again. Export exactly the visible, sorted results to CSV.
3. **Close-to-tray.** Settings include `close_to_tray`; if a tray is available, a close request hides the window without terminating sampling. The tray menu allows restoring the window or quitting explicitly. If the tray cannot initialize, closing terminates normally and the settings page explains why.
4. **Opt-in privileged network sampling.** Installing NetHogs is optional. Enabling it may open the system Polkit dialog; cancelling leaves metric unavailable and shows a diagnostic. A child monitor must be stopped when disabled or when exiting. No shell command interpolation.
5. **Configuration.** Settings persist in the user's OS configuration directory as JSON. Invalid polling intervals normalize to a safe default. GUI supports 1, 2, 5 seconds.
6. **Predictable resource accounting.** Network frames are not integrated more than once, and process identifier reuse cannot transfer the previous process's byte totals. No unbounded history buffers. A pause control halts CPU/process collection, without losing UI controls.
7. **No silent platform claims.** When the OS or driver does not provide a signal, show `—` plus a status explanation. Never derive per-PID network traffic from system-wide network traffic.
8. **Delivery.** GitHub Actions on each push to `main` runs formatting, lint, unit tests and builds on Windows and Linux. Only after both builds succeed, attach both binaries to a unique pre-release for that push.

## Quality and security boundaries

- Read-only process monitoring; no process termination, file scanning, credential acquisition, keystroke capture, or network packet persistence.
- Keep all process/network raw metadata in memory; exporting CSV is an explicit user action.
- No telemetry or automatic internet communication from app code.
- System-wide totals include traffic from all active non-loopback network interfaces and may contain VPN / virtual interfaces (double counting possible).
- App requires a desktop session (Windows or Linux X11/Wayland with a tray implementation); WSL/headless support is not in scope.
- UI and tray must not require administrator privileges; only NetHogs is elevated on Linux.

## Test-first plan (TDD)

- Domain: formatting, missing data, sort direction, filter semantics, aggregation.
- Network: parse NetHogs rows and reject non-process records; combine duplicate PID rows.
- GPU: parse fdinfo and NVIDIA pmon responses; keep unknown states explicit.
- Accounting: deduplicate NetHogs frame sequence and isolate reused PIDs by `(pid, start_time)`.
- Settings: validate malformed interval, preserve close-to-tray across JSON serialization.
- Export: preserve column order, UTF-8 text, quoting and missing values.
- CI: compilation and tests on Linux and Windows; emit release only after checks pass.

## Known limits / planned future work

- Native Windows per-PID network via ETW (requires privileged provider/session setup).
- Native Windows per-process GPU via PDH GPU Engine counters.
- Persistent time-series database and multi-session totals, with retention controls.
- Signed releases and end-to-end tray tests in real desktop sessions.
- Per-GPU normalization for multi-GPU machines and network interface selection.

A passing CI pipeline is necessary, not sufficient, for production-readiness. Desktop runtime smoke tests (especially permission prompts and tray on KDE GNOME and Windows Explorer) are required before declaring a stable release.
