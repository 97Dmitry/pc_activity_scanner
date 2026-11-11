# Windows desktop integration

## Approved scope

Improve readability (body/buttons 15, secondary 13, table numbers 14, chart axes
12), increase text/control contrast, provide original window/desktop/tray icons.
User additionally approved opt-in Windows per-process network capture with UAC,
then requested Windows login startup with an optional administrator mode.

## Decisions

- GUI remains unelevated by default. Network capture uses a separate helper;
  no persistent service or hidden installation. User must explicitly opt in.
- Windows ETW TCP/UDP byte events feed existing bounded accounting/history.
  Process identity, event loss, startup failure and cancellation must preserve
  unknowns; never distribute global traffic across applications.
- Use an authenticated local IPC channel with bounded messages. Close helper
  when the UI exits/disables capture. Never stop another application's ETW session.
- Startup config is per user, uses interactive Task Scheduler login tasks and
  queries actual OS state. Full-GUI administrator startup is optional, default off;
  elevated registration must not change the intended user on alternate UAC credentials.
- No claim of retroactive traffic: older intervals remain unknown.
- Existing root-owned Linux NetHogs path and protected-main PR/release workflow
  remain required. Ship a new numbered release after both platform checks.

## Additional approved UI fixes

User reported periodic flashing and an unreadable active segment in a 12-hour
chart. Keep old displayed data through background refresh, add 5/15/30-minute
periods and drag-to-zoom; plot labels use 14 and adaptive tick counts. Default
period is 15 minutes. Old data must never acquire a new query label or export.

## Implementation ownership

Root: readability, artwork, UI integration, documentation, release.
windows_traffic: ETW helper, IPC, capture lifecycle, network/collector accounting.
windows_autostart: OS startup backend and configuration lifecycle.

## Validation

Portable parsing/accounting/security tests, Windows cross compilation, Rust fmt,
Clippy and all tests, packaging tests, Linux GUI inspection at 1400x860/960x600,
Windows/Linux CI. Review new privilege boundaries before integration. WSL/Xvfb
cannot establish real Windows UAC, Task Scheduler, live ETW or Explorer acceptance;
release notes must disclose this and give a concrete manual checklist.

## Completed verification

- Linux: 94 tests passed, one explicit fixture generator ignored; separately run
  fixture generator passed. Formatting, Clippy, packaging tests and actionlint pass.
- Complete Windows GNU cross-target check/Clippy pass, including bundled SQLite
  and embedded EXE icon resource. Native Windows/MSVC CI passes the IPC token
  level and read-only Task Scheduler query tests.
- Windows production-helper runtime smoke passes TCP/UDP IPv4/IPv6 traffic
  accounting and confirms its ETW session disappears after disable.
- Native read-only Windows PowerShell protected-path checks accept known system
  executables; task creation/UAC/logon remain manual acceptance.
- Xvfb: 1400x860 and 960x600 process/settings layouts; controlled 12-hour fixture,
  default short period, actual pointer zoom, two-app comparison and 80 sampled
  chart frames across background refresh without disappearance.
- Independent reviews found and fixed pipe token impersonation, moved-EXE startup
  repair and successful-empty-query/shared-warning confusion; scoped re-review
  accepted the fixes. No schema change to historical storage.
