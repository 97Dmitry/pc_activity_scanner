# Architecture

The application is a local desktop monitor. It samples resource counters; it
never controls other processes or sends their metadata to a service.

## Responsibilities

| Module | Responsibility |
|---|---|
| `main` | Native window setup and platform capture-helper entry points |
| `app` | User commands, settings, tray actions and screen state |
| `ui` | Navigation, overview/history/comparison, interactive plots and virtualized process table |
| `history` | Duration-weighted buckets, SQLite store, schema/retention and bounded worker/service |
| `history_export` | Explicit asynchronous CSV export of a selected historical result |
| `model` | Metric types, search, grouping, sorting and cached visible rows |
| `collector` | Background sysinfo sampling and process-lifetime traffic ledger |
| `network` | NetHogs/Windows ETW sources, bounded frame delivery and helper lifetime |
| `gpu`, `nvidia` | Driver-dependent DRM/NVIDIA sampling |
| `autostart` | Asynchronous Windows Task Scheduler query/configuration with original-user and protected-path checks |
| `icon` | Embedded window/tray artwork; Windows EXE resource via build script |
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
OS data. The table virtualizes rows; its short live chart has at most 120 samples.
Persistent analytics are independent of GUI observation and use a separate
bounded SQLite pipeline; see [HISTORY.md](HISTORY.md).

Search matches original process names, PIDs and executable paths **before**
name grouping. CPU/RAM/activity thresholds apply to the resulting groups.
CSV exports use exactly the same cached rows shown in the table.

## Persistent history

Each completed collector snapshot carries a UTC acquisition timestamp, a process
start identity and a monotonic contiguous interval. Initial/resume intervals are
zero. The recorder sends the immutable snapshot reference through a bounded
channel; the collector never waits for SQL. The history worker groups executable
identities, computes 30-second weighted averages/peaks and commits transactions.
Counter deltas isolate process lifetimes and avoid repeated NetHogs integration.

The worker alone owns the SQLite connection, schema/app ID, retention, clear and
queries. Defaults retain 24 hours; settings support 1/3/7 days. Local data uses
owner-private Unix permissions. A 512-MiB main-database limit and bounded queues
prevent unbounded buffers; status exposes storage errors and dropped samples.

UI queries include request IDs and selected ranges/application IDs. Results are
immutable shared references; the UI matches IDs before showing/exporting data and
throttles automatic refresh. Old queries cannot overwrite the selected period.
Plots show coverage and break at missing/partial buckets rather than drawing
invented zeros. Up to four selected application series share a comparison axis.
History export runs outside rendering and preserves unknowns and true timestamps.

## Lifetimes and platform boundaries

The collector owns a stop flag shared with its NVIDIA sampler. A separate GUI
wake timer allows tray events to be handled while the native window is hidden.
NetHogs shutdown closes the elevated helper's stdin and reaps the process in a
background worker, leaving UI commands responsive. Old-session callbacks must
not change a newer session's status or rates.

Only the installed, root-owned Linux helper may be elevated. Every ancestor of
its executable must be root-owned and protected against group/other writes.
The GUI stays unprivileged by default. Windows opt-in ETW network capture runs
in a separate UAC helper with a unique session and authenticated bounded local
IPC. Epoch checks prevent stopped sessions overwriting new state. Lost events
or helper failure make network data unavailable. GPU remains unsupported.

Windows startup is managed by an asynchronous per-user Task Scheduler adapter.
It queries actual OS state rather than treating JSON as authoritative. Original
user SID is preserved across UAC. Full-GUI elevated startup is optional and
prepares a protected, per-user, digest-addressed copy after explicit consent.
The source SHA256 is pinned before UAC and checked during installation; fixed
managed directories and their owners/ACLs/markers are verified. Existing running
images are never replaced. Raw shell errors stay in diagnostics, while the
normal flow retains confirmed state and recovery controls.
Task configuration never runs on the GUI thread. See `docs/DOWNLOADS.md`.

## Verification boundaries

Unit tests cover parsing, filtering, caching, accounting, configuration and
export contracts. CI runs formatting, Clippy, tests and release builds on Linux
and Windows with the toolchain pinned in `rust-toolchain.toml`. Version-tag
releases package both validated binaries with documentation and SHA256 checksums;
normal main pushes retain CI artifacts without publishing. See `RELEASING.md`.
A virtual X11
smoke test can establish startup and window resizing; it does not prove actual
Windows, Wayland, system tray or Polkit behavior. Use `QA_CHECKLIST.md` for
those checks before declaring a stable release.
