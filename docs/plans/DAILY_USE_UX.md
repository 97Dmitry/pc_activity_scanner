# Daily-use desktop UX

## User-reported defects

- CPU contribution bars use the same accent for text and fill.
- Per-app network capture is hidden behind technical settings despite visible
  global traffic; missing permission/capture is mistaken for broken metrics.
- Elevated startup rejects the executable path, exposes raw CLIXML/PowerShell,
  then discards actual state and disables the controls needed to recover.
- Settings mix everyday choices with implementation details and passive strings.

## Approved direction

Keep the established theme and desktop architecture. Move CPU share values outside
the bar, show recommended full-monitoring setup on the main resource screens,
remember explicit permission/disable choices, and provide inline enable/retry.
Do not silently start UAC before explaining why Windows asks for permission.
CPU/RAM/global networking continue while per-app capture is unavailable.

Everyday settings show real controls and concise actual states. Put paths,
backend/driver names, raw errors and storage details in an explicit diagnostics
window. Actionable errors remain visible in friendly language with recovery.

Windows startup reads actual OS configuration. Ordinary login startup is a
simple control; elevated startup is an explicit action that prepares a managed,
protected executable copy with UAC instead of asking users to manage ACLs.
Hash-pin the source before elevation, validate the fixed destination chain,
reject reparse/unrelated locations, commit immutable digest-named files atomically and preserve original SID.
Never weaken permissions or grant arbitrary privileged commands.

## Validation

Portable migration/consent/recovery/security tests, native Windows API/PowerShell
checks where read-only, reviewed disposable CI task round-trip if safe, existing
ETW smoke and Linux GUI checks at 960x600 and 1400x860. Inspect CPU ranking,
first-run capture states, ordinary settings, error recovery and diagnostic window.
Do not mutate host Windows tasks or install files from agent diagnostics.

## Ownership

Root: CPU contribution readability, docs, review and release.
startup_product: startup backend, managed-copy safety and structured errors.
daily_ui: settings, consent/status/actions and diagnostic window.

## Verification completed before native CI

104 Linux tests passed, one explicit fixture generator ignored; fixture generator
was separately run successfully. Full Windows GNU all-target Clippy and Linux
release build pass. Actionlint and five packaging tests pass.

Native Xvfb inspection covered first-run enable/skip, persisted skip, inline
unavailable/retry/details, CPU ranking at 960x600 and 1400x860, ordinary settings,
and F12 diagnostics. Raw unavailable-component detail remained in diagnostics;
normal controls stayed available. CPU values are outside the fill at 0.8%,
33.1% and 66.1% in the controlled fixture.

Independent review accepted protected immutable installation, SID/ACL/hash
checks, error recovery, and the exact opt-in disposable CI namespaces/cleanup.
Read-only actual Windows PowerShell parsing/ACL checks pass; neither agents nor
default tests changed host tasks, files in Program Files, or permissions.
The two explicit Windows CI roundtrips must pass before release. Real UAC/logon
and Explorer acceptance remain separately documented.

## Native round-trip finding

The first disposable Windows round-trip created tasks, then exposed raw account
name versus SID comparison in ownership validation. Principal/trigger identities
are now resolved to canonical SIDs while the exact Source marker remains
mandatory; unknown identities fail closed. Read-only native identity fixtures
and scoped review pass. The native round-trips must rerun after this fix.

Network availability also uses frame freshness rather than a shared availability
flag alone: collector expiration can run on a cloned sample. The read-only
predicate and regression preserve unavailable state for old/missing frames.
