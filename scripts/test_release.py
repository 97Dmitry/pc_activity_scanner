import hashlib
import importlib.util
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "Cargo.toml").write_text('[package]\nname = "pc_activity_scanner"\nversion = "0.2.0"\n')
        (self.root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "pc_activity_scanner"\nversion = "0.2.0"\n')
        (self.root / "docs/releases").mkdir(parents=True)
        (self.root / "docs/releases/v0.2.0.md").write_text("Release notes")
        (self.root / "README.md").write_text("Developer instructions", encoding="utf-8")
        (self.root / "docs/DOWNLOADS.md").write_text("Инструкция", encoding="utf-8")
        (self.root / "LICENSE").write_text("MIT")
        self.artifacts = self.root / "dist"
        self.artifacts.mkdir()
        (self.artifacts / "pc_activity_scanner-linux-x86_64").write_bytes(b"linux binary")
        (self.artifacts / "pc_activity_scanner-windows-x86_64.exe").write_bytes(b"windows binary")

    def test_rejects_mismatching_or_nonversion_tags(self):
        self.assertEqual(release.validate_version(self.root, "v0.2.0"), "0.2.0")
        for tag in ["v0.1.0", "snapshot-abc", "v0.2.0-beta", "v0.2.0/../../other"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.validate_version(self.root, tag)

    def test_requires_matching_lockfile_and_release_notes(self):
        lockfile = self.root / "Cargo.lock"
        lockfile.write_text(lockfile.read_text().replace('version = "0.2.0"', 'version = "0.1.0"'))
        with self.assertRaises(ValueError):
            release.validate_version(self.root, "v0.2.0")
        lockfile.write_text(lockfile.read_text().replace('version = "0.1.0"', 'version = "0.2.0"'))
        (self.root / "docs/releases/v0.2.0.md").unlink()
        with self.assertRaises(ValueError):
            release.validate_version(self.root, "v0.2.0")

    def test_archives_contain_binaries_instructions_and_license(self):
        output = self.root / "release-assets"
        # Windows chmod does not preserve POSIX execute bits; tar metadata must.
        with patch.object(Path, "chmod", return_value=None):
            release.package_release(self.root, self.artifacts, output, "v0.2.0")
        with zipfile.ZipFile(output / "pc-activity-scanner-v0.2.0-windows-x86_64.zip") as archive:
            prefix = "pc-activity-scanner-v0.2.0-windows-x86_64/"
            self.assertEqual(archive.read(prefix + "pc_activity_scanner.exe"), b"windows binary")
            self.assertEqual(archive.read(prefix + "LICENSE"), b"MIT")
            self.assertIn("Инструкция", archive.read(prefix + "README.md").decode("utf-8"))
        with tarfile.open(output / "pc-activity-scanner-v0.2.0-linux-x86_64.tar.gz") as archive:
            prefix = "pc-activity-scanner-v0.2.0-linux-x86_64/"
            binary = archive.getmember(prefix + "pc-activity-scanner")
            self.assertEqual(binary.mode & 0o777, 0o755)
            self.assertEqual(archive.extractfile(binary).read(), b"linux binary")
            self.assertEqual(archive.extractfile(prefix + "LICENSE").read(), b"MIT")
        manifest = (output / "SHA256SUMS.txt").read_text().splitlines()
        self.assertEqual(len(manifest), 4)
        for line in manifest:
            digest, filename = line.split("  ")
            self.assertEqual(hashlib.sha256((output / filename).read_bytes()).hexdigest(), digest)

    def test_missing_platform_does_not_publish_partial_package(self):
        (self.artifacts / "pc_activity_scanner-windows-x86_64.exe").unlink()
        output = self.root / "release-assets"
        with self.assertRaises(ValueError):
            release.package_release(self.root, self.artifacts, output, "v0.2.0")
        self.assertFalse(output.exists())

    def test_existing_output_cannot_be_silently_overwritten(self):
        output = self.root / "release-assets"
        output.mkdir()
        (output / "keep").write_bytes(b"keep")
        with self.assertRaises(ValueError):
            release.package_release(self.root, self.artifacts, output, "v0.2.0")
        self.assertEqual((output / "keep").read_bytes(), b"keep")


if __name__ == "__main__":
    unittest.main()
