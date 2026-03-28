#!/usr/bin/env python3

from __future__ import annotations

import argparse
import math
import os
import shutil
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError as exc:  # pragma: no cover - hard failure on old Python
    raise SystemExit("Python 3.11+ is required to run this packager") from exc


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "target" / "debian"
DEFAULT_DESCRIPTION = (
    "Rust TUI/CLI wrapper for Claude, Gemini, Codex, and Copilot with SQLite history"
)
DEFAULT_MAINTAINER = "Secret Agent Maintainers <noreply@example.com>"


def parse_args() -> argparse.Namespace:
    manifest = load_manifest(REPO_ROOT / "Cargo.toml")
    package_name = manifest["package_name"]
    binary_name = manifest["binary_name"]

    parser = argparse.ArgumentParser(
        description="Package an existing Rust build as a Debian .deb archive."
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=REPO_ROOT / "target" / "release" / binary_name,
        help="Path to the built Rust binary to package.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help="Directory where the staging tree and resulting .deb will be written.",
    )
    parser.add_argument(
        "--package-name",
        default=package_name,
        help="Debian package name to use in the control file.",
    )
    parser.add_argument(
        "--install-name",
        default=binary_name,
        help="File name to install under /usr/bin inside the package.",
    )
    parser.add_argument(
        "--version",
        default=manifest["version"],
        help="Debian package version.",
    )
    parser.add_argument(
        "--architecture",
        default=detect_architecture(),
        help="Debian architecture string, for example amd64.",
    )
    parser.add_argument(
        "--maintainer",
        default=detect_maintainer(manifest.get("authors", [])),
        help="Maintainer value for the Debian control file.",
    )
    parser.add_argument(
        "--description",
        default=manifest["description"],
        help="Short package description for the Debian control file.",
    )
    parser.add_argument(
        "--section",
        default="utils",
        help="Debian package section.",
    )
    parser.add_argument(
        "--priority",
        default="optional",
        help="Debian package priority.",
    )
    return parser.parse_args()


def load_manifest(path: Path) -> dict[str, object]:
    with path.open("rb") as handle:
        data = tomllib.load(handle)

    package = data.get("package", {})
    package_name = package.get("name")
    version = package.get("version")
    if not package_name or not version:
        raise SystemExit(f"missing package name or version in {path}")

    binaries = data.get("bin", [])
    if binaries and isinstance(binaries, list):
        binary_name = binaries[0].get("name", package_name)
    else:
        binary_name = package_name

    description = package.get("description") or DEFAULT_DESCRIPTION
    authors = package.get("authors", [])
    return {
        "package_name": str(package_name),
        "binary_name": str(binary_name),
        "version": str(version),
        "description": str(description),
        "authors": authors if isinstance(authors, list) else [],
    }


def detect_architecture() -> str:
    dpkg = shutil.which("dpkg")
    if not dpkg:
        raise SystemExit("dpkg is required to determine the Debian architecture")

    result = subprocess.run(
        [dpkg, "--print-architecture"],
        check=True,
        capture_output=True,
        text=True,
    )
    architecture = result.stdout.strip()
    if not architecture:
        raise SystemExit("dpkg returned an empty architecture string")
    return architecture


def detect_maintainer(authors: list[object]) -> str:
    full_name = os.environ.get("DEBFULLNAME")
    email = os.environ.get("DEBEMAIL")
    if full_name and email:
        return f"{full_name} <{email}>"

    if authors:
        first_author = str(authors[0]).strip()
        if first_author:
            return first_author

    return DEFAULT_MAINTAINER


def format_control(description: str, args: argparse.Namespace, binary_size: int) -> str:
    lines = [
        f"Package: {args.package_name}",
        f"Version: {args.version}",
        f"Section: {args.section}",
        f"Priority: {args.priority}",
        f"Architecture: {args.architecture}",
        f"Maintainer: {args.maintainer}",
        f"Installed-Size: {max(1, math.ceil(binary_size / 1024))}",
        f"Description: {description.strip()}",
    ]
    return "\n".join(lines) + "\n"


def ensure_built_binary(binary_path: Path) -> None:
    if not binary_path.is_file():
        raise SystemExit(f"built binary not found: {binary_path}")
    if not os.access(binary_path, os.X_OK):
        raise SystemExit(f"binary is not executable: {binary_path}")


def build_package(args: argparse.Namespace) -> Path:
    dpkg_deb = shutil.which("dpkg-deb")
    if not dpkg_deb:
        raise SystemExit("dpkg-deb is required to build Debian packages")

    binary_path = args.binary.resolve()
    ensure_built_binary(binary_path)

    output_dir = args.output_dir.resolve()
    staging_dir = output_dir / f"{args.package_name}_{args.version}_{args.architecture}"
    deb_path = output_dir / f"{args.package_name}_{args.version}_{args.architecture}.deb"
    binary_destination = staging_dir / "usr" / "bin" / args.install_name
    control_path = staging_dir / "DEBIAN" / "control"

    if staging_dir.exists():
        shutil.rmtree(staging_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    (staging_dir / "DEBIAN").mkdir(parents=True)
    binary_destination.parent.mkdir(parents=True)

    shutil.copy2(binary_path, binary_destination)
    binary_destination.chmod(0o755)
    control_path.write_text(
        format_control(args.description, args, binary_path.stat().st_size),
        encoding="utf-8",
    )
    control_path.chmod(0o644)

    if deb_path.exists():
        deb_path.unlink()

    subprocess.run(
        [dpkg_deb, "--build", "--root-owner-group", str(staging_dir), str(deb_path)],
        check=True,
    )
    return deb_path


def main() -> int:
    args = parse_args()
    deb_path = build_package(args)
    print(deb_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
