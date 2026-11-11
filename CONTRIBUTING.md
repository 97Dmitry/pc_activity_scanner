# Contributing

We use **spec-driven development and test-driven development**.

1. Describe behavior and acceptance criteria in the relevant issue or docs/SPEC.md before implementation.
2. Add a failing unit/regression test for every reproducible defect in the parser, accounting or settings layer.
3. Implement the smallest correct change and run:
   - cargo fmt --all -- --check
   - cargo clippy --all-targets --locked -- -D warnings
   - cargo test --all-targets --locked
   - cargo build --release --locked
4. Open a PR for a peer review; CI builds and tests both Ubuntu and Windows.
5. Do not merge unless the acceptance criteria are satisfied; a green CI build is not a substitute for a real OS-specific smoke test.

## Privilege boundary

Never run the desktop GUI as root. The opt-in Linux NetHogs helper is only enabled from a root-owned installed binary under protected system directories; the development build has privileged collection disabled by design. Review all changes to that helper as security-sensitive.

## Releases

Every passing push to main produces a commit-specific pre-release with Linux and Windows executables and SHA256 checksums. Signatures and stable release criteria are tracked in GitHub issues.
