# Architecture

The application is a local desktop monitor. It samples resource counters; it
never controls other processes or sends their metadata to a service.

## Responsibilities

| Module | Responsibility |
|---|---|
| `main` | Native window setup and Linux helper entry point |
| `app` | User commands, settings, tray actions and screen state |
| `ui` | Theme, summary/history rendering and virtualized process table |
| `model` | Metric types, search, grouping, sorting and cached visible rows |
| `collector` | Background sysinfo sampling and process-lifetime traffic ledger |
| `network` | NetHogs parser, bounded frame delivery and helper lifetime |
| `gpu`, `nvidia` | Driver-dependent DRM/NVIDIA sampling |
| `settings` | Validated JSON configuration and safe file replacement |
| `export` | Explicit CSV export with quoting and formula protection |
| `tray` | Native tray lifecycle and menu events |

## Data flow

The network reader publishes both the latest rates and a bounded queue of
completed frames. The collector consumes each pending frame once, independent
of the configured 1/2/5-second process polling interval. Totals are keyed by
`(pid, start_time)` and pruned when a process exits. Attribution is still an
estimate: a process which exits between process samples may be absent, and
NetHogs does not expose a process-start timestamp.

The collector publishes immutable snapshots through a shared `Arc`. The GUI
clones a snapshot reference and rebuilds its visible rows only when the
snapshot, search, filters or sort changes. Sorting and filtering never request
OS data. The table virtualizes rows; history is limited to 120 samples. The
history axis uses elapsed time, including polling interval changes and pauses.

Search matches original process names, PIDs and executable paths **before**
name grouping. CPU/RAM/activity thresholds apply to the resulting groups.
CSV exports use exactly the same cached rows shown in the table.

## Lifetimes and platform boundaries

The collector owns a stop flag shared with its NVIDIA sampler. A separate GUI
wake timer allows tray events to be handled while the native window is hidden.
NetHogs shutdown closes the elevated helper's stdin and reaps the process in a
background worker, leaving UI commands responsive. Old-session callbacks must
not change a newer session's status or rates.

Only the installed, root-owned Linux helper may be elevated. Every ancestor of
its executable must be root-owned and protected against group/other writes.
The GUI stays unprivileged. Windows currently provides CPU, RAM and aggregate
network counters; unsupported per-process metrics remain `None` / `—`.

## Verification boundaries

Unit tests cover parsing, filtering, caching, accounting, configuration and
export contracts. CI runs formatting, Clippy, tests and release builds on Linux
and Windows with the toolchain pinned in `rust-toolchain.toml`. A virtual X11
smoke test can establish startup and window resizing; it does not prove actual
Windows, Wayland, system tray or Polkit behavior. Use `QA_CHECKLIST.md` for
those checks before declaring a stable release.
