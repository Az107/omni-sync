"""Release contract: tag/manifest agreement and complete, verified assets (Python 3.11+)."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile
import tomllib
import zipfile


ROOT = Path(__file__).resolve().parents[1]
TARGETS = [
    ("ubuntu-latest", "x86_64-unknown-linux-gnu", True),
    ("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", True),
    ("macos-latest", "aarch64-apple-darwin", True),
    ("macos-latest", "x86_64-apple-darwin", False),
    ("windows-latest", "aarch64-pc-windows-msvc", False),
    ("windows-latest", "x86_64-pc-windows-msvc", True),
]


def release_plan(tag, root=ROOT):
    match = re.fullmatch(r"(client|server)-v(\d+\.\d+\.\d+)", tag)
    if not match:
        raise ValueError("expected a stable client-vX.Y.Z or server-vX.Y.Z tag")
    component, version = match.groups()
    package = "omni-cli" if component == "client" else "omni-server"
    binary = "omni-sync" if component == "client" else "omni-sync-server"
    with (root / "crates" / package / "Cargo.toml").open("rb") as source:
        actual = tomllib.load(source)["package"]["version"]
    if actual != version:
        raise ValueError(f"{tag} disagrees with {package} manifest version {actual}")
    targets = TARGETS if component == "client" else TARGETS[:2]
    return {
        "component": component,
        "version": version,
        "targets": [
            dict(os=os, target=target, package=package, bin=binary, native=native)
            for os, target, native in targets
        ],
    }


def asset_pairs(plan):
    for target in plan["targets"]:
        stem = f'{target["bin"]}-{plan["version"]}-{target["target"]}'
        windows = "windows" in target["target"]
        yield (
            stem + (".exe" if windows else ""),
            stem + (".zip" if windows else ".tar.gz"),
            target["bin"] + (".exe" if windows else ""),
        )


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def validate_assets(plan, directory, checksums=False):
    pairs = list(asset_pairs(plan))
    expected = {name for bare, archive, _ in pairs for name in (bare, archive)}
    actual = {path.name for path in directory.iterdir()}
    allowed = expected | ({"SHA256SUMS"} if checksums else set())
    if actual != allowed:
        raise ValueError(f"asset mismatch: missing={allowed - actual}, unexpected={actual - allowed}")
    for bare, archive, member in pairs:
        if (directory / bare).is_symlink() or not (directory / bare).is_file():
            raise ValueError(f"not a regular binary: {bare}")
        if not (directory / bare).stat().st_size:
            raise ValueError(f"empty binary: {bare}")
        if (directory / archive).is_symlink():
            raise ValueError(f"archive is a symlink: {archive}")
        if archive.endswith(".zip"):
            with zipfile.ZipFile(directory / archive) as bundle:
                if bundle.namelist() != [member]:
                    raise ValueError(f"unexpected archive members: {archive}")
                with bundle.open(member) as source:
                    archived_hash = hashlib.file_digest(source, "sha256").hexdigest()
        else:
            with tarfile.open(directory / archive) as bundle:
                members = bundle.getmembers()
                if len(members) != 1 or members[0].name != member or not members[0].isfile():
                    raise ValueError(f"unexpected archive members: {archive}")
                if members[0].mode & 0o111 != 0o111:
                    raise ValueError(f"archive binary is not executable: {archive}")
                with bundle.extractfile(members[0]) as source:
                    archived_hash = hashlib.file_digest(source, "sha256").hexdigest()
        if archived_hash != digest(directory / bare):
            raise ValueError(f"archive differs from bare binary: {archive}")
    sums = "".join(f"{digest(directory / name)}  {name}\n" for name in sorted(expected))
    if checksums:
        if (directory / "SHA256SUMS").read_text() != sums:
            raise ValueError("SHA256SUMS does not match the complete asset set")
    else:
        (directory / "SHA256SUMS").write_text(sums)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("--dist", type=Path)
    parser.add_argument("--verify-checksums", action="store_true")
    args = parser.parse_args()
    try:
        plan = release_plan(args.tag)
        if args.dist:
            validate_assets(plan, args.dist, args.verify_checksums)
        else:
            for key, value in plan.items():
                print(f"{key}={json.dumps(value) if isinstance(value, list) else value}")
    except (ValueError, OSError, tarfile.TarError, zipfile.BadZipFile) as error:
        parser.exit(1, f"release validation failed: {error}\n")


if __name__ == "__main__":
    main()
