# Persistent activity analysis — v0.3.0

Approved scope: 12-hour persistent application resource history, 1/6/12/24-hour
views, resource overview, application ranking, selected-application charts,
comparison of up to four applications and CSV export. The user delegates visual
and technical choices. This builds on v0.2.0; keep that release independent.

## Product contract

- Record while the application runs, including hidden-to-tray operation. Never
  imply collection while the app/PC was stopped. Pause leaves a visible gap.
- Default retention 24 hours; settings allow 1/3/7 days and disabling collection.
  Existing history remains readable when recording is disabled. Clear is a
  separate confirmed UI action. Store local process names/paths and resource
  metrics only, no browsing/window/keyboard content.
- Aggregate into 30-second buckets with duration-weighted averages and separate
  peaks. CPU/RAM are always present for observed processes. Unknown GPU/network
  values remain optional; missing is not zero. Coverage is explicit.
- Identify applications by executable path; fallback to name for inaccessible
  paths. Aggregate multiple instances before temporal aggregation. PID reuse
  cannot transfer resource totals to another process lifetime.
- SQLite owns persistence, migrations and queries on one worker thread. No SQL
  or disk I/O in the GUI or collector. A bounded sample channel drops with an
  explicit counter instead of blocking sampling. Cap main database at 512 MiB;
  report storage errors while live monitoring continues. Retention and orphan
  metadata pruning are application operations, never manual database edits.
- UI: Overview / Processes / History / Compare / Settings. Dark slate surfaces,
  cyan CPU, amber memory, violet GPU and green network. Dense readable tables,
  restrained metric cards, local-time axes, hover values/crosshair, clear
  recording/empty/loading/error states. Charts break at missing samples.
- Ranges can move backward/forward; Now returns to rolling history. Search and
  ranking controls, application detail and comparison work on exited apps too.
- CSV exports the selected time window with true timestamps, empty unknowns,
  spreadsheet-safe names/paths and collision-free filenames. It is labelled
  as historical data and does not claim to be a live snapshot export.
- Do not add process termination, autostart services, ETW/PDH, cloud sync,
  energy estimates or productivity/focus tracking in this iteration.

## Boundaries and interfaces

`src/history/types.rs` is the shared contract. `HistoryService` in `history/mod.rs`
provides start(config), recorder(), request(query)->bool, configure(config)->bool,
clear()->bool, view()->Arc<HistoryView>. HistoryRecorder is Clone and provides
record(Arc<Snapshot>) without waiting on SQLite. HistoryQuery carries request_id,
from/to Unix seconds and at most four application IDs. GUI matches response
request_id before displaying/exporting it. HistoryConfig defaults enabled=true,
retention_days=1. Worker flushes its partial bucket on orderly shutdown with a
bounded wait; a crash may lose the current uncommitted 30-second bucket.

Collector snapshots gain sampled_at: i64 (UTC Unix seconds) and sample_seconds:
f64 (actual monotonic interval, zero for initial/resume samples). Only contiguous
samples enter history. ProcessRow remains the source of CPU/RAM/GPU/network
metrics. Network history uses differences of cumulative per-lifetime counters
so a shared two-second NetHogs frame is not integrated twice at one-second
polling. Include process started_at for this purpose. Offline/unknown metrics
are not interpolated. Query results are bounded by retention and point limits.

## Responsibilities

- `history/types.rs`: shared renderer-independent contracts.
- `history`: SQL schema/store, aggregation, bounded service and persistence tests.
- `collector`, `model`, `settings`, `main`: capture timing, process lifetime,
  nonblocking recorder delivery and configuration compatibility.
- `app`, `ui/history`, `history_export`: navigation, asynchronous queries,
  interaction, gap-aware charts, confirmation and report export.

## Verification of v0.3.0 preparation

- 79 unit/regression tests pass, including time weighting, independent metric
  coverage, counter dedup/reset/PID reuse, pause, dropped samples, retention,
  clear/config/shutdown ordering, rollback and database failure cases.
- The ignored fixture generator was run explicitly through application code.
  A separate 12-hour/reopen regression verifies 1440 buckets/43200 seconds and
  an independent six-hour query. This does not claim an actual overnight run.
- Linux Clippy with warnings denied, rustfmt, packaging tests and actionlint pass.
- Linux Xvfb: 960×600 and 1400×860, actual sampling, orderly exit/restart,
  historical CSV, application selection, comparison and confirmed/cancelled clear.
- New Unix history directory/file/lock permissions verified as 0700/0600/0600.
- Independent collector and whole-feature reviews completed; UTC rollback,
  queued configuration on shutdown and duplicate-descriptor lock lifetime
  defects were reproduced and fixed with regressions. Ten consecutive complete
  suite runs passed after the lock fix.
- Native Windows checks are required in the PR and tagged-release CI. A real
  Windows GUI, Wayland, GPU/Polkit/tray and an actual 12-hour session remain
  manual acceptance; no source document claims those checks passed.

## Decisions

SQLite is bundled for portable Windows/Linux builds. Thirty-second aggregates
preserve sampled peaks without storing every repaint; default 24-hour retention
supports a 12-hour analysis while bounding normal storage. One worker owns all
SQL and one file lock prevents overlapping writers. Persisted UTC high-water
prevents recounting previously covered time after clock rollback or restart.
Experimental schema 1 migrates conservatively to schema 2; published v0.2.0 had
no persistent history. Release publication follows `../RELEASING.md` and must
verify both platform jobs and actual assets for the exact tagged SHA.
