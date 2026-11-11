"""Validate versioned releases and package the two CI-built executables."""

import argparse
import hashlib
from pathlib import Path
import re
import shutil
import tarfile
import tempfile
import tomllib
import zipfile


def linux_tar_permissions(info: tarfile.TarInfo) -> tarfile.TarInfo:
    info.mode = 0o755 if info.isdir() or info.name.endswith("/pc-activity-scanner") else 0o644
    return info


def validate_version(root: Path, tag: str) -> str:
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Release tag must have the form vMAJOR.MINOR.PATCH")
    version = tag[1:]
    package = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    locked = next(p for p in lock["package"] if p["name"] == package["name"])
    if package["version"] != version or locked["version"] != version:
        raise ValueError("Release tag, Cargo.toml and Cargo.lock versions must match")
    if not (root / "docs/releases" / f"{tag}.md").is_file():
        raise ValueError(f"Release notes are missing for {tag}")
    return version


def package_release(root: Path, artifacts: Path, output: Path, tag: str) -> None:
    validate_version(root, tag)
    binaries = {
        "linux-x86_64": artifacts / "pc_activity_scanner-linux-x86_64",
        "windows-x86_64": artifacts / "pc_activity_scanner-windows-x86_64.exe",
    }
    documents = {
        "README.md": root / "docs/DOWNLOADS.md",
        "LICENSE": root / "LICENSE",
        "RELEASE_NOTES.md": root / "docs/releases" / f"{tag}.md",
    }
    for source in [*binaries.values(), *documents.values()]:
        if not source.is_file():
            raise ValueError(f"Required release input is missing: {source}")
    if output.exists():
        raise ValueError(f"Refusing to overwrite an existing release directory: {output}")
    output.mkdir(parents=True)
    for platform, source in binaries.items():
        windows = platform.startswith("windows")
        extension = ".exe" if windows else ""
        standalone = output / f"pc_activity_scanner-{tag}-{platform}{extension}"
        shutil.copyfile(source, standalone)
        if not windows:
            standalone.chmod(0o755)
        folder_name = f"pc-activity-scanner-{tag}-{platform}"
        with tempfile.TemporaryDirectory(dir=output.parent) as temporary:
            folder = Path(temporary) / folder_name
            folder.mkdir()
            executable = folder / ("pc_activity_scanner.exe" if windows else "pc-activity-scanner")
            shutil.copyfile(standalone, executable)
            if not windows:
                executable.chmod(0o755)
            for name, document in documents.items():
                shutil.copyfile(document, folder / name)
            if windows:
                with zipfile.ZipFile(output / f"{folder_name}.zip", "w", zipfile.ZIP_DEFLATED) as archive:
                    for file in sorted(folder.iterdir()):
                        archive.write(file, f"{folder_name}/{file.name}")
            else:
                with tarfile.open(output / f"{folder_name}.tar.gz", "w:gz") as archive:
                    archive.add(folder, arcname=folder_name, filter=linux_tar_permissions)
    manifest = "".join(
        f"{hashlib.sha256(file.read_bytes()).hexdigest()}  {file.name}\n"
        for file in sorted(output.iterdir())
    )
    (output / "SHA256SUMS.txt").write_text(manifest, encoding="utf-8", newline="\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["validate", "package"])
    parser.add_argument("tag")
    parser.add_argument("--artifacts", type=Path, default=Path("dist"))
    parser.add_argument("--output", type=Path, default=Path("release-assets"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    if args.command == "validate":
        print(validate_version(root, args.tag))
    else:
        package_release(root, args.artifacts, args.output, args.tag)
        for file in sorted(args.output.iterdir()):
            print(file.name)


if __name__ == "__main__":
    main()
