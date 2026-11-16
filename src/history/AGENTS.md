# Resource history

- Hold the exclusive `history.sqlite3.lock` File lock for the complete Store
  lifetime; concurrent GUI instances must never add overlapping resource totals.
  Guard Drop explicitly unlocks after SQLite closes: closing one descriptor alone
  leaves the lock held when a forked child or clone still owns a duplicate.
- `types.rs` is the shared UI/collector contract. SQLite and filesystem work stay
  on `HistoryService`'s worker; recorder/command queues must remain bounded.
- Traffic uses raw `(pid, started_at)` cumulative-counter deltas. Reset baselines
  at unknowns, counter resets, pause, disable/clear, queue loss and time gaps.
  Never integrate repeated NetHogs rates as new per-process bytes.
- Persist the exact UTC high-water mark with each flush. Seed it on restart and
  preserve it through pause/configuration resets; only successful clear resets it.
  Skip overlapping whole intervals and refresh their counter baselines.
- Advance bucket time from monotonic sample durations anchored to UTC. Integer
  timestamps alone overlap fractional intervals. Preserve independent GPU and
  network coverage; one unknown instance makes that app's metric unknown.
- Generation changes can arrive through the sample queue before their commands;
  defer them within a bounded queue and replay after applying the command.
- `seed_history_fixture` is an ignored application-code test; the explicit
  `PC_ACTIVITY_HISTORY_FIXTURE` path receives synthetic data for GUI acceptance.
- Schema v3 `extra_metrics` stores supplementary measurements with independent
  duration/weighted sums/peaks; old buckets have no invented coverage. Keep
  retention, clear and partial-flush transactions consistent with base buckets.
  Rate integrals are bytes only for I/O; memory integrals are byte-seconds.
- Native process creation tokens join PID and UTC start in recorder baselines;
  absent/changed lifetimes cannot contribute an initial supplementary interval.
- Schema v4 adds independent CPU/RAM coverage. Presence duration must not become
  CPU/RAM coverage when any grouped process is unknown. Legacy rows inherit their
  old presence coverage because earlier false zeros cannot be reconstructed.
