# PC Activity Scanner — product specification (SDD)

Status: versioned desktop release. Version: 0.5.0. Supported signals are listed below.

## Objective

A local desktop utility, written in Rust, that attributes resource consumption to running processes and allows the user to find expensive applications quickly. No cloud accounts, background uploads, or analytics.

## Supported platforms and data contracts

| Signal | Linux desktop | Windows desktop |
|---|---|---|
| Total CPU/RAM | `sysinfo` | `sysinfo` |
| Per-process CPU/RAM | `sysinfo` | `sysinfo` |
| Total network RX/TX rate | `sysinfo::Networks` | `sysinfo::Networks` |
| Per-process RX/TX rate | NetHogs trace; opt-in, `pkexec`/root | Opt-in ETW TCP/UDP helper, UAC |
| Per-process bytes since started | Integrate each NetHogs frame once (estimate) | Observed ETW TCP/UDP byte deltas, process lifetime checked |
| Per-process GPU use | DRM fdinfo, NVIDIA pmon fallback (driver dependent) | **Unavailable**; show `—` |
| GPU dedicated VRAM | DRM fdinfo, NVIDIA pmon if exposed | **Unavailable**; show `—` |

Linux CPU percentages are normalized across logical cores (100% = all logical CPU capacity). Network rate units are bytes/s, total bytes are accumulated **in the present app session**, not lifetime totals. The network backend may not attribute traffic to some processes. Unknown metrics must be represented as unknown, never as zero. Per-process network rates expire after seven seconds without a new NetHogs frame. GPU percentage reflects available vendor signals and is not directly comparable across drivers.

## User scenarios and acceptance criteria

1. **Live overview.** Open the app and see total CPU, used/total memory, total RX/TX rate and a sorted process table. The UI must remain responsive while sampling runs in worker threads.
2. **Explore usage.** Search original process names, paths and PIDs before grouping same-name matches. Apply activity/network/CPU/RAM thresholds to the resulting rows or groups. Basic mode shows CPU/RAM/network rates; detailed mode adds GPU and session totals. Sort by clicking any metric header and flip direction by clicking again. Export exactly the visible, sorted results to CSV without spreadsheet formula injection or overwriting existing files. Reuse filtered/sorted rows until the snapshot or view inputs change.
3. **Close-to-tray.** Settings include `close_to_tray`; if a tray is available, a close request hides the window without terminating sampling. The tray menu allows restoring the window or quitting explicitly. If the tray cannot initialize, closing terminates normally and the settings page explains why.
4. **Opt-in privileged network sampling.** Installing NetHogs is optional. Enabling it may open the system Polkit dialog; cancelling leaves metric unavailable and shows a diagnostic. A child monitor must be stopped when disabled or when exiting. No shell command interpolation.
5. **Configuration.** Settings persist in the user's OS configuration directory as JSON. Replacement uses a unique temporary file in the same directory and preserves the previous file until replacement succeeds. Invalid polling intervals normalize to a safe default. GUI supports 1, 2, 5 seconds.
6. **Predictable resource accounting.** Consume every queued NetHogs frame once, even when process polling is slower than its two-second capture period. Bound the pending queue at 256 frames; discard oldest frames on overflow and report the gap in metric status. Key totals by `(pid, start_time)` and reject frames acquired before the current process started. NetHogs does not expose process lifetime identity; same-second PID reuse remains ambiguous. The short live chart has at most 120 samples. Persistent history uses 30-second weighted-average/peak buckets in a local SQLite database, independently of GUI repainting. Pause halts CPU/process collection; NetHogs/NVIDIA continue independently. On resume, global network speed uses the actual interval including the pause. Stopping NetHogs must not wait for helper exit on the GUI thread; callbacks from an old session cannot overwrite a newer session.
7. **No silent platform claims.** When the OS or driver does not provide a signal, show `—` plus a status explanation. Never derive per-PID network traffic from system-wide network traffic.
8. **Delivery.** GitHub Actions on each push to `main` runs formatting, lint, unit tests and builds on Windows and Linux with Rust 1.95.0. Linux also checks window startup/resizing with Xvfb. Normal pushes do not publish releases. Version tags matching Cargo.toml/Cargo.lock and a commit on main publish a numbered Latest release only after both builds pass, with release notes, ready-to-run Windows/Linux archives, standalone binaries and SHA256 checksums.

9. **Historical analysis.** Provide Overview, History and Compare views with 1/6/12/24-hour ranges, previous/next/Now navigation, application search/ranking and selected-application detail. Include exited apps. Compare at most four apps on a shared scale. Render local time axes, hover values and missing-data gaps. Show observed coverage, not just the requested duration.
10. **History durability.** Capture on the collector thread through a bounded nonblocking recorder channel into a single SQLite worker, including hidden-window use. Initial/resume samples reset continuity. Use executable identity and per-lifetime network counter deltas; unknown/reset/gap counters must not be charged as real zero or double-integrated. Keep live monitoring usable after storage failures. Enforce schema ownership/version checks and a 512-MiB main-database page limit. Preserve committed history across restart; orderly shutdown flushes a partial bucket with bounded waiting, crashes may lose the uncommitted bucket.
11. **History export.** Export the selected loaded historical window separately from live CSV, with real UTC/local timestamps and metric coverage. Unknown values stay empty; names/paths remain formula-safe and filenames collision-free.

## Quality and security boundaries

- Read-only process monitoring; no process termination, file scanning, credential acquisition, keystroke capture, or network packet persistence.
- Persist application names/paths and resource aggregates locally with default 24-hour retention, optional 1/3/7-day retention and recording toggle. Keep raw snapshots in memory. Clearing saved history requires a separate confirmation; CSV export remains explicit.
- No telemetry or automatic internet communication from app code.
- System-wide totals include traffic from all active non-loopback network interfaces and may contain VPN / virtual interfaces (double counting possible).
- App requires a desktop session (Windows or Linux X11/Wayland with a tray implementation); WSL/headless support is not in scope.
- UI and tray do not require administrator privileges; network helpers are elevated separately. Optional Windows administrator login startup is explicit and requires a protected executable location.

## Test-first plan (TDD)

- Domain: formatting, missing data, sort direction, filter semantics, aggregation.
- Network: parse NetHogs rows and reject non-process records; combine duplicate PID rows.
- GPU: parse fdinfo and NVIDIA pmon responses; keep unknown states explicit.
- Accounting: deduplicate NetHogs frame sequence and isolate reused PIDs by `(pid, start_time)`.
- Settings: validate malformed interval, preserve close-to-tray across JSON serialization.
- Export: preserve column order, UTF-8 text, quoting, missing values; neutralize spreadsheet formulas and avoid filename collisions.
- CI: compilation and tests on Linux and Windows; emit release only after checks pass.

## Known limits / planned future work

- Native Windows per-process GPU via PDH GPU Engine counters.
- Longer-term rollups beyond the current bounded 1/3/7-day history and native runtime coverage.
- Signed releases and end-to-end tray tests in real desktop sessions.
- Per-GPU normalization for multi-GPU machines and network interface selection.

A passing CI pipeline is necessary, not sufficient, for production-readiness. Desktop runtime smoke tests (especially permission prompts and tray on KDE GNOME and Windows Explorer) are required before claiming production readiness; a versioned release must state the checks actually completed.
