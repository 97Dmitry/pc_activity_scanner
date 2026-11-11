# Security policy

## Reporting vulnerabilities

Please use **GitHub → Security → Report a vulnerability** (private security advisory) where this feature is enabled. Otherwise contact the repository owner privately instead of publishing proof-of-concept exploit details in a public issue.

## Threat model

- The application samples local resource counters; it does not inspect keys, visited websites, credentials or packet payloads and contains no telemetry endpoint.
- Detailed Linux traffic requires NetHogs. Polkit grants root permission only to a short-lived capture helper, not the GUI.
- The helper executable and every ancestor directory must be root-owned and not writable by group/other. An executable from a writable development directory cannot be elevated.
- The helper stops its NetHogs child if its parent UI closes the stdin pipe; the UI bounds shutdown waits.
- Exported CSV filenames are unique and spreadsheet formulas are neutralized before writing.
- GPU and network counters may be unavailable because of driver limitations or process permissions; absence is never reported as zero.

## Still required before stable release

OS-native runtime smoke tests, security review of helper and packaging, SBOM/dependency audit, signed artifacts and Windows native per-PID network/GPU backends are tracked as open GitHub issues. Snapshot pre-releases should be considered unverified on real end-user desktops.
