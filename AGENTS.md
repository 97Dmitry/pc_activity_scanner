# Project conventions

- Desktop Rust/egui monitor for Linux and Windows. Keep platform-specific APIs
  behind `cfg(target_os = ...)`; both CI builds are required.
- `model` owns filtering, grouping, sorting and display caching. `collector`
  owns sysinfo sampling and `(pid, start_time)` traffic accounting. `network`
  owns NetHogs parsing, frame delivery and helper lifetime. UI must not wait
  for subprocess shutdown or perform resource sampling.
- Unknown per-process metrics use `None` / `—`; never infer them from global
  counters. NetHogs frames must be accounted once, including frames between
  slower UI collection ticks. Bound queues and history buffers.
- GUI runs without elevation. The Linux capture helper must remain root-owned
  with protected ancestors; do not weaken this for development convenience.
- Tests belong beside the Rust implementation. For accounting/parsers/settings,
  reproduce defects before fixing them. Required checks: `cargo fmt --all --
  --check`, `cargo clippy --all-targets --locked -- -D warnings`,
  `cargo test --all-targets --locked`, `cargo build --release --locked`.
- Keep `README.md`, `docs/SPEC.md` and `docs/QA_CHECKLIST.md` consistent with
  supported metrics. Virtual-display tests do not establish Windows/tray/Polkit
  acceptance. Snapshots are prereleases, not stable releases.
- Work in the current checkout. Do not commit, push or change repository
  protection without explicit authorization.
