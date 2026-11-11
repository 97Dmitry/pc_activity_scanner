# Contributing

Use the pinned toolchain in `rust-toolchain.toml` (rustup installs the required
compiler and components). See [Architecture](docs/ARCHITECTURE.md) for module
ownership and data contracts. We use **spec-driven development and test-driven development**.

1. Describe behavior and acceptance criteria in the relevant issue or docs/SPEC.md before implementation.
2. Add a failing unit/regression test for every reproducible defect in the parser, accounting or settings layer.
3. Implement the smallest correct change and run:
   - cargo fmt --all -- --check
   - cargo clippy --all-targets --locked -- -D warnings
   - cargo test --all-targets --locked
   - cargo build --release --locked
4. Open a PR for a peer review; CI builds and tests both Ubuntu and Windows.
5. Do not merge unless the acceptance criteria are satisfied; a green CI build is not a substitute for a real OS-specific smoke test.

On Linux, install `xvfb`, `xauth` and `xdotool`, then run
`LIBGL_ALWAYS_SOFTWARE=1 timeout 30s xvfb-run -a bash scripts/smoke-linux.sh`.
The script uses a temporary configuration directory and verifies window startup
and resizing to 960×600. Set `SCANNER_SCREENSHOT=/absolute/path.png` to capture
the window too (requires ImageMagick). This does not validate real tray/Polkit
integration or Windows behavior.

## Privilege boundary

Never run the desktop GUI as root. The opt-in Linux NetHogs helper is only enabled from a root-owned installed binary under protected system directories; the development build has privileged collection disabled by design. Review all changes to that helper as security-sensitive.

## Releases

Releases are published from version tags after both platform checks pass.
Follow [RELEASING.md](docs/RELEASING.md) for version changes, release notes,
packaging and publication. Normal main pushes do not publish snapshots.
Main is protected: use a PR with both platform checks passing; force pushes and
deletion are disabled, including for administrators.
