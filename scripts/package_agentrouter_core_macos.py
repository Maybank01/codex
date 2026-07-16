#!/usr/bin/env python3
"""Package a signed macOS Codex Core as an immutable Runtime v2 component."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import struct
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from pathlib import Path


TARGETS = {
    "aarch64-apple-darwin": ("arm64", 0x0100000C),
    "x86_64-apple-darwin": ("x64", 0x01000007),
}
MACHO_64_MAGICS = {b"\xcf\xfa\xed\xfe": "little", b"\xfe\xed\xfa\xcf": "big"}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sanitize_version(value: str) -> str:
    result = re.sub(r"[^0-9A-Za-z._-]+", "-", value.strip()).strip("-")
    if not result:
        raise ValueError("Version cannot be represented in an artifact filename")
    return result


def macho_cpu_type(path: Path) -> int:
    with path.open("rb") as stream:
        header = stream.read(8)
    if len(header) != 8 or header[:4] not in MACHO_64_MAGICS:
        raise ValueError(f"Core binary is not a thin 64-bit Mach-O executable: {path}")
    byte_order = "<" if MACHO_64_MAGICS[header[:4]] == "little" else ">"
    return struct.unpack(f"{byte_order}I", header[4:8])[0]


def run(*args: str, cwd: Path | None = None) -> str:
    completed = subprocess.run(
        args,
        cwd=cwd,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return completed.stdout.strip()


def git_clean(repo_root: Path) -> bool:
    return not run(
        "git", "status", "--porcelain", "--untracked-files=all", cwd=repo_root
    )


def codesign_authority(binary: Path) -> str:
    completed = subprocess.run(
        ["codesign", "--display", "--verbose=4", str(binary)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        return "unsigned"
    for line in completed.stderr.splitlines():
        if line.startswith("Authority="):
            return line.removeprefix("Authority=").strip()
    return "ad-hoc"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--output-directory", type=Path)
    parser.add_argument("--compatible-shell-version", action="append", default=[])
    parser.add_argument("--allow-dirty", action="store_true")
    parser.add_argument("--require-developer-id", action="store_true")
    parser.add_argument("--notarization-expected", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parents[1]
    cargo_root = repo_root / "codex-rs"
    release_config = json.loads(
        (repo_root / "scripts" / "agentrouter-core-release.json").read_text(
            encoding="utf-8"
        )
    )
    cargo_manifest = tomllib.loads(
        (cargo_root / "Cargo.toml").read_text(encoding="utf-8")
    )
    version = str(cargo_manifest["workspace"]["package"]["version"])
    if version != str(release_config["version"]):
        raise ValueError(
            f"Cargo version {version} does not match release config {release_config['version']}"
        )

    binary = args.binary.resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"Core binary is missing: {binary}")
    arch, expected_cpu_type = TARGETS[args.target]
    actual_cpu_type = macho_cpu_type(binary)
    if actual_cpu_type != expected_cpu_type:
        raise ValueError(
            f"Mach-O CPU type 0x{actual_cpu_type:08x} does not match {args.target} "
            f"(expected 0x{expected_cpu_type:08x})"
        )

    clean = git_clean(repo_root)
    if not clean and not args.allow_dirty:
        raise ValueError(
            "Refusing to package a release component from a dirty worktree"
        )
    source_sha = run("git", "rev-parse", "HEAD", cwd=repo_root)
    upstream_sha = str(release_config["upstream_sha"])
    subprocess.run(
        ["git", "merge-base", "--is-ancestor", upstream_sha, "HEAD"],
        cwd=repo_root,
        check=True,
    )

    version_output = run(str(binary), "--version")
    expected_version_output = f"codex-cli {version}"
    if version_output != expected_version_output:
        raise ValueError(
            f"Unexpected Core version output {version_output!r}; expected {expected_version_output!r}"
        )

    authority = codesign_authority(binary)
    if args.require_developer_id and not authority.startswith(
        "Developer ID Application:"
    ):
        raise ValueError(
            f"Core binary is not Developer ID signed; observed authority: {authority}"
        )

    compatible_shells = args.compatible_shell_version or list(
        release_config.get("compatible_shell_versions", [])
    )
    if not compatible_shells:
        raise ValueError("At least one compatible Desktop Shell version is required")

    output_directory = (
        args.output_directory or repo_root / "dist" / "agentrouter-core"
    ).resolve()
    output_directory.mkdir(parents=True, exist_ok=True)
    safe_version = sanitize_version(version)
    artifact_name = f"CodexCore-mac-{arch}-{safe_version}.zip"
    archive_path = output_directory / artifact_name
    checksum_path = output_directory / f"{artifact_name}.sha256"
    if archive_path.exists() or checksum_path.exists():
        raise FileExistsError(f"Immutable Core output already exists: {archive_path}")

    binary_sha256 = sha256_file(binary)
    binary_size = binary.stat().st_size
    manifest = {
        "schemaVersion": 1,
        "kind": "agentrouter-codex-core",
        "platform": "macos",
        "arch": arch,
        "version": version,
        "entrypoint": "codex",
        "activation": {
            "mode": "external",
            "environmentVariable": "CODEX_CLI_PATH",
        },
        "compatibleShellVersions": compatible_shells,
        "upstreamGitSha": upstream_sha,
        "upstreamRepository": str(release_config["upstream_repository"]),
        "sourceGitSha": source_sha,
        "sourceDirty": not clean,
        "rustTarget": args.target,
        "codeSignatureAuthority": authority,
        "notarizationExpected": bool(args.notarization_expected),
        "files": [{"path": "codex", "size": binary_size, "sha256": binary_sha256}],
    }

    with tempfile.TemporaryDirectory(prefix="agentrouter-core-macos-") as temp:
        staging = Path(temp)
        staged_binary = staging / "codex"
        staged_binary.write_bytes(binary.read_bytes())
        staged_binary.chmod(0o755)
        (staging / "agentrouter-core.json").write_text(
            json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
        )
        with zipfile.ZipFile(
            archive_path, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=6
        ) as archive:
            archive.write(staging / "agentrouter-core.json", "agentrouter-core.json")
            archive.write(staged_binary, "codex")

    archive_sha256 = sha256_file(archive_path)
    checksum_path.write_text(f"{archive_sha256}  {artifact_name}\n", encoding="utf-8")
    print(
        json.dumps(
            {
                "archivePath": str(archive_path),
                "checksumPath": str(checksum_path),
                "archiveSha256": archive_sha256,
                "binarySha256": binary_sha256,
                "version": version,
                "arch": arch,
                "signatureAuthority": authority,
            }
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
