# Versioned releases

Normal pushes to `main` and pull requests run CI and retain build artifacts;
they do not publish releases. A tag `vMAJOR.MINOR.PATCH` publishes a numbered
GitHub release marked Latest after Windows and Linux checks pass.

## Preparing a release

1. In a working branch, update the package version in `Cargo.toml` and
   `Cargo.lock`. Update the download filenames in README.
2. Write `docs/releases/vMAJOR.MINOR.PATCH.md`: changes, which ready-to-run
   assets to download, verification and known limitations. Do not claim manual
   desktop tests that were not performed.
3. Run normal Rust checks and `python -m unittest discover -s scripts -p test_release.py`.
   Release scripts require Python 3.11+ and only its standard library.
4. Open a PR into `main`. Main requires both platform checks, resolved review
   conversations and linear history. No second reviewer is mandatory for this
   single-maintainer repository. Administrators follow the same protections;
   force pushes and deletion are disabled.
5. After the PR is merged, check out and fast-forward `main`, then create the
   version tag on that exact commit. For example:

   ```bash
   git switch main
   git pull --ff-only
   python scripts/release.py validate v0.3.0
   git tag -a v0.3.0 -m "PC Activity Scanner 0.3.0"
   git push origin v0.3.0
   ```

## What CI enforces

- Tag syntax and equality with package/lockfile versions; release notes exist.
- The tagged commit is reachable from `origin/main`.
- Packaging tests, formatting, Clippy, Rust tests and release builds on both OSes.
- A Linux Xvfb startup/resizing smoke test.
- Both binaries must exist before packaging. Linux tar permissions include an
  executable binary; archives include README, LICENSE and release notes.
- SHA256SUMS covers both archives and both standalone binaries.
- The release is published from its existing tag and is not a prerelease.
  Existing releases are not silently overwritten. Never move a published tag;
  fix an error in a new patch release.

After publication verify the workflow's exact SHA, all release assets and the
checksums. Check that `/releases/latest` resolves to the new version. Legacy
`snapshot-*` prereleases remain for history and are no longer generated.
