# Windows traffic capture

- `windows.rs` owns the fixed-function UAC helper, ETW session and named-pipe
  lifecycle; `windows_protocol.rs` owns bounded, validated wire frames and event
  prefix parsing. Do not add privileged commands over this data-only channel.
- Both pipe peers validate the expected process identity. Elevated pipe clients
  must use `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`; default impersonation
  would let the unprivileged server borrow the helper's administrator token.
- Each ETW session has a unique GUID/name. Never stop a pre-existing session or
  another application's logger. Event loss makes capture unavailable.
- Count exact event bytes once, keyed by PID and exact FILETIME process creation
  identity. Header PID is not network-event payload PID. Missing/unreadable PIDs
  are unknown, not inferred zero. Keep protocol/accounting tests and Windows
  native TCP/UDP IPv4/IPv6 smoke coverage when changing these contracts.
