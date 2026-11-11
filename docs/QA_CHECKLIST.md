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

## Persistent history / analytics
- [ ] Observe graphs across multiple 15-second background refreshes; data does not disappear and scroll position stays stable.
- [ ] At 960×600 and larger, 14px axes/ticks remain readable; drag either direction to zoom (min60s), verify ranking/CSV share the range, then reset to15 minutes.
- [ ] A fresh profile shows live resources and an honest empty history; points appear as recording proceeds.
- [ ] Record for over a minute, exit normally, restart and verify stored points/applications remain.
- [ ] 5/15/30-minute and 1/6/12/24-hour ranges and Earlier/Later/Now select the displayed window; exited apps remain inspectable.
- [ ] Ranking identifies executables with equal names/different paths, and distinguishes CPU contribution from observed-time CPU average.
- [ ] Select an app, inspect CPU/RAM/GPU/network plots, compare up to four apps, change metrics and search.
- [ ] Hover reports actual bucket times/values; CPU/GPU axis stays 0–100; offline, unknown and partial intervals are not bridged.
- [ ] Pause/resume (including rapid toggles), hidden window and Settings do not misattribute or interrupt intended recording.
- [ ] Disable recording, browse existing history, set 1/3/7-day retention, and confirm clear; cancel preserves data.
- [ ] Export a historical result; query timestamps, coverage, unknown blanks, spreadsheet text protection and distinct filenames are correct.
- [ ] Unwritable/invalid/locked database shows an error while live processes remain available; second history writer is handled safely.
- [ ] Long synthetic 12-hour fixtures/reopen/query tests pass; a real overnight 12-hour session remains a separate acceptance check.

## Security / delivery
- [ ] Windows: enable per-app ETW capture, accept/cancel UAC, check real TCP/UDP traffic and historical byte totals; disabled/unavailable intervals remain unknown.
- [ ] Windows: IPv4/IPv6 loopback runtime smoke passes in CI; real browser downloads remain a separate desktop check.
- [ ] Windows: disable capture and close GUI while UAC is pending; late confirmation must not leave an active capture session.
- [ ] Windows: lost ETW events/helper failure show unavailable, and stale callbacks cannot overwrite a newer capture session.
- [ ] Windows: configure/remove ordinary login startup, re-query state, sign out/in, verify interactive UI and one task per user.
- [ ] Windows: from Downloads, explicit elevated setup creates a protected managed copy and Highest task; no manual folder steps. Cancel UAC and recover without disabled controls.
- [ ] Managed install pins source hash, rejects tampering/reparse/unrelated markers; repeat same version and retarget a new version while old managed EXE runs.
- [ ] Fresh and migrated profiles offer visible full monitoring; Enable/Skip/Retry/explicit Disable persist coherently and never distribute global traffic across apps.
- [ ] Settings show functional controls; raw CLIXML, paths, driver/backend details appear only in Diagnostics with Copy. Errors preserve actual startup state and usable recovery.
- [ ] CPU contribution value is readable for 0%, partial and full bars at 960×600.
- [ ] Windows: move portable EXE, re-query stale task, repair/downgrade/remove it; unrelated task collisions fail closed.
- [ ] Windows: alternate UAC credentials do not change task principal; standard user cannot select automatic Highest.
- [ ] Windows: inspect icon in Explorer/shortcut/taskbar/tray at 100/125/150/200% DPI, dark/light taskbar; no generic EXE icon.
- [ ] Windows: unsupported GPU metrics continue showing `—`.
- [ ] No outbound telemetry or background updates; no logs containing packet payloads or private window content.
- [ ] All Actions jobs pass for each release SHA; binary artifacts download and launch.
- [ ] Release uses a version tag matching Cargo.toml/Cargo.lock, includes both platform archives and standalone binaries with SHA256 checksums, and appears as Latest. Release notes disclose known limits and completed checks; code signing remains planned.
