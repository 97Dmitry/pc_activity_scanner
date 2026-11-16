# Project conventions

- Desktop Rust/egui monitor for Linux and Windows. Keep platform-specific APIs
  behind `cfg(target_os = ...)`; both CI builds are required.
- `model` owns filtering, grouping, sorting and display caching. `collector`
  owns sysinfo sampling and `(pid, start_time)` traffic accounting. `network`
  owns NetHogs/Windows ETW parsing, frame delivery and helper lifetime. UI must not wait
  for subprocess shutdown or perform resource sampling.
- `history` owns aggregation, all SQLite access/migrations and bounded worker
  queues. Collector snapshots carry UTC time, contiguous duration and process
  lifetime identity; never record history only from GUI repainting. See
  `docs/HISTORY.md` and `docs/plans/ACTIVITY_HISTORY.md` for coverage semantics.
- `process_details` and `windows_processes` own authoritative process lifetime,
  CPU and memory availability. Windows uses a bounded native snapshot including
  minimal VMs; Linux validates stat/status/exe/stat. Preserve first-sample RAM
  with unknown CPU, exact lifetime and host-core normalization. Only vmmemWSL
  is labelled WSL2; no elevation. Private commit, resident and swap differ.
- `ProcessView` caches expanded name groups and separate parent trees. Parent
  links require exact creation tokens; presentation rows never enter recording.
  Expanded CSV includes group/process row_kind: never double-sum summaries.
- Unknown per-process metrics use `None` / `—`; never infer them from global
  counters. NetHogs frames must be accounted once, including frames between
  slower UI collection ticks. Bound queues and history buffers.
- GUI runs without elevation by default; the explicitly selected Windows Highest
  startup mode is the exception. The Linux capture helper must remain root-owned
  with protected ancestors; do not weaken this for development convenience.
- Windows `autostart` queries actual per-user Task Scheduler state. Preserve the
  original user SID across UAC, validate task ownership, and require a protected
  executable/ancestor ACL chain for Highest. Never expose arbitrary privileged
  commands or perform task/PowerShell work on the GUI thread. Highest startup
  uses fixed protected per-SID digest-addressed copies: pin source SHA256 before
  UAC, verify copy/ACL/markers, never weaken permissions or replace a running EXE.
  Compare Task Scheduler principal/trigger identities by resolved SID (Windows
  may return an account name); never weaken the exact Source/owner guard.
  Old managed releases are retained. Mutating Windows CI tests are opt-in/ignored,
  use unique test namespaces and must never change host or production tasks.
- History query failures are distinct from recording/storage warnings. Keep the
  displayed query and snapshot paired during background refresh and CSV export.
- Tests belong beside the Rust implementation. For accounting/parsers/settings,
  reproduce defects before fixing them. Required checks: `cargo fmt --all --
  --check`, `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --all-targets --locked`, `cargo build --release --locked`.
- Keep `README.md`, `docs/SPEC.md` and `docs/QA_CHECKLIST.md` consistent with
  supported metrics. Virtual-display tests do not establish Windows/tray/Polkit
  acceptance. Version tags publish releases; normal main pushes do not. See
  `docs/RELEASING.md`; tags, Cargo versions and release notes must match.
- Keep `eframe`'s `clipboard` feature enabled when disabling its default features.
  Without it, Copy buttons and selected-text copying use an app-only fallback,
  silently failing to reach the OS clipboard. Verify copying into another app.
- Main is protected, including admins. Use PRs and both CI checks; never bypass
  protection or force-update published version tags.
- Work in the current checkout. Do not commit, push or change repository
  protection without explicit authorization.
- Linux GPU samples must carry native lifetime tokens; cached PID-only pmon
  data cannot be assigned to a newer process. Driver failures clear cache.
- `connections` owns bounded selected-lifetime socket queries off GUI. Keep
  selection/result generation paired; no DNS, packet contents or inferred
  connection byte counters. Linux inode ownership is checked around table reads.
- Traffic totals use PID/UTC start/native token on both OSes. NetHogs remains
  an estimate: birth must precede the entire actual frame, not just delayed
  publication. Missing native identity is unknown; ETW requires exact wire token.
- `thermal` samples named sources on its worker; ACPI zones are not CPU sensors.
  Thermal transitions and current NVIDIA thermal flags have different meaning.
  `command` bounds unprivileged CLI output/deadlines and owns child cleanup;
  do not reintroduce blocking/unbounded Command::output in samplers.

- `alerts` evaluates only collector snapshots. Preserve a settings revision so
  off/on between ticks resets dwell; missing metrics and PID reuse never advance
  a rule. Journal100/state32768/rules16 remain bounded. Desktop delivery has its
  own bounded generation-checked worker; do not issue OS notifications from GUI.
- Graceful quit signals collector/delivery and polls completion, never joins on
  GUI. Preserve command parent-death ownership and the documented Windows birth
  race rather than adding arbitrary privileged process management.
- Draw process disclosure indicators with egui geometry, not Unicode arrow
  characters: the bundled proportional font lacks those glyphs. Preserve the
  shared triangle/name click target and text-only names in copy/export.
