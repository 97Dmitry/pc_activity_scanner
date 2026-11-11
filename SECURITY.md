# Security policy

## Reporting vulnerabilities

Please use **GitHub → Security → Report a vulnerability** (private security advisory) where this feature is enabled. Otherwise contact the repository owner privately instead of publishing proof-of-concept exploit details in a public issue.

## Threat model

- The application samples local resource counters; it does not inspect keys, visited websites, credentials or packet payloads and contains no telemetry endpoint.
- Detailed Linux traffic requires NetHogs. Polkit grants root permission only to a short-lived capture helper, not the GUI.
- The helper executable and every ancestor directory must be root-owned and not writable by group/other. An executable from a writable development directory cannot be elevated.
- The helper stops its NetHogs child if its parent UI closes the stdin pipe; the UI bounds shutdown waits.
- Exported CSV filenames are unique and spreadsheet formulas are neutralized before writing.
- Persistent history stores local process names/paths and resource aggregates in the per-user data directory, with bounded retention and explicit clear/disable controls. New Unix history files/directories are owner-private. The application checks database ownership/schema before migrations; GUI and collector do not run SQL.
- GPU and network counters may be unavailable because of driver limitations or process permissions; absence is never reported as zero.

## Delivery verification and remaining work

Native desktop runtime checks, helper/packaging security review, dependency
inventory/audit and signed artifacts remain work before production-readiness
claims. Releases state their completed checks and limitations explicitly;
passing CI does not establish real desktop/tray/Polkit acceptance. Windows
per-PID network/GPU backends are separate feature work, not implied by a
version number. Published files are currently unsigned; SHA256SUMS verifies
file integrity, not publisher identity.
