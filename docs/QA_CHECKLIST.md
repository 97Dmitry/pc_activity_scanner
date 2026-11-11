# Manual acceptance / production smoke-test checklist

This project is not declared production-ready until the following pass on actual machines and the results are recorded in a release issue.

## Build / startup
- [ ] Windows 11 x64 clean user profile, no development tools: packaged `.exe` launches normally, icon visible.
- [ ] Kubuntu (Plasma + Wayland) on a non-root account: Linux binary launches, renders Cyrillic font, loads processes.
- [ ] No crash on an empty/default config, invalid JSON settings or unavailable network backend.
- [ ] CPU usage and memory of scanner itself remain within an acceptable budget at 1-second sampling.

## Process accounting
- [ ] CPU % across processes is normalized against all logical cores and reasonable against Task Manager / `top`.
- [ ] RAM usage matches one known process within OS accounting differences (resident vs. working set).
- [ ] Total network RX/TX rates move during a large download and approach zero when idle.
- [ ] Linux NetHogs (if installed): enabling prompts for authentication; denial does not freeze the UI.
- [ ] Linux capture from a user-writable build is denied; the root-owned installed binary is accepted.
- [ ] Linux NetHogs disabled: elevated helper actually exits, no stale per-process measurements remain.
- [ ] Linux NetHogs: force-close the GUI and verify the privileged helper terminates its NetHogs child after stdin EOF.
- [ ] Linux NetHogs: terminate NetHogs unexpectedly and verify UI reports collector unavailable instead of silently keeping old values.
- [ ] Linux NetHogs enabled: e.g. browser download has plausible per-PID rates; exported session totals do not jump every repaint.
- [ ] GPU load on integrated/optional NVIDIA GPU visible when driver provides per-process data; otherwise `—` rather than a fake 0.
- [ ] No leaked helper processes after quit; no noticeable growth in memory over a 4-hour idle session.

## UI / preferences
- [ ] Search by executable name, PID and path, including international text.
- [ ] With grouping enabled, search a child PID and a path shared by only one same-name process; only matching processes contribute to the result.
- [ ] At 960×600, navigation and status stay visible; settings scroll; basic/detailed tables and context-menu copying work.
- [ ] Empty search results explain how to reset filters; first startup and pause show clear status.
- [ ] Sort all 10 columns ascending/descending; unknown metrics stay at the bottom.
- [ ] Toggle filters and name grouping while processes enter/exit; no crash or flickering.
- [ ] Change polling 1/2/5 seconds; reload persisted settings after restart.
- [ ] Close-to-tray ON: window hides and sampling continues; restore menu reopens; Quit terminates.
- [ ] Close-to-tray OFF: window close exits the app; no invisible background process remains.
- [ ] Tray unavailable: app still exits on close; settings provide a useful message.
- [ ] Pause and resume with process churn; no spurious spikes.
- [ ] NetHogs at 5-second polling: all two-second frames contribute to session totals. A pause longer than 8.5 minutes reports dropped frames; totals are then incomplete estimates.
- [ ] Rapidly disable/re-enable NetHogs: UI stays responsive and old helper errors/EOF do not clear the new session.
- [ ] Export twice rapidly: both CSVs exist. Verify UTF-8 Cyrillic, delimiter, sorting, CSV escaping and Excel formula safety.

## Security / delivery
- [ ] On Windows, only CPU/RAM and global networking have values: unsupported per-PID metrics show `—`.
- [ ] No outbound telemetry or background updates; no logs containing packet payloads or private window content.
- [ ] All Actions jobs pass for each release SHA; binary artifacts download and launch.
- [ ] Release uses a unique SHA tag and is marked pre-release; SHA256 checksums are included, but code signing remains planned.
