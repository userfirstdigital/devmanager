#!/usr/bin/env python3
"""Assemble an allowlisted debug app/host archive for desktop acceptance."""
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import zipfile

root = Path(__file__).resolve().parent.parent
target = (root / "target").resolve()
if not target.is_relative_to(root) or target == root:
    raise SystemExit("Candidate target escaped the isolated checkout")
if Path(os.environ.get("CARGO_TARGET_DIR", target)).resolve() != target:
    raise SystemExit("Candidate target differs from the isolated build target")
system = platform.system().lower()
if system not in ("windows", "linux"):
    raise SystemExit("Candidate acceptance is configured for Windows and Linux")
architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(
    platform.machine().lower(), platform.machine().lower()
)
read_env = dict(os.environ, GIT_OPTIONAL_LOCKS="0")
commit = subprocess.check_output(
    ["git", "rev-parse", "HEAD"], cwd=root, env=read_env, text=True
).strip()
subprocess.run(["git", "diff", "--exit-code", "HEAD", "--"], cwd=root, env=read_env, check=True)
files = []
suffix = ".exe" if system == "windows" else ""
for name in ("devmanager", "devmanager-host"):
    source = target / "debug" / (name + suffix)
    if source.is_symlink() or not source.is_file() or not source.resolve().is_relative_to(target):
        raise SystemExit(f"Missing or redirected candidate binary: {name}")
    with source.open("rb") as handle:
        magic = handle.read(4)
    if not (magic[:2] == b"MZ" if system == "windows" else magic == b"\x7fELF"):
        raise SystemExit(f"Candidate binary is not native to {system}: {name}")
    files.append((source, name + suffix, 0o755))
for resource in ("assets", "third_party/ghostty"):
    base = root / resource
    if base.is_symlink() or not base.is_dir():
        raise SystemExit(f"Invalid resource root: {resource}")
    for source in sorted(base.rglob("*")):
        if source.is_symlink() or not source.resolve().is_relative_to(base):
            raise SystemExit("Redirected candidate resource")
        if source.is_file():
            files.append((source, source.relative_to(root).as_posix(), 0o644))

def digest(path):
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()

readme = """DevManager desktop acceptance candidate

Extract the whole archive, then run launch-devmanager.sh (Linux) or
launch-devmanager.cmd (Windows). Keep both binaries and resources together.
The launcher uses an isolated debug profile inside this extracted directory.
This is a test candidate, not a signed release installer. Review candidate.json
for source and file hashes.

Linux requires a graphical desktop, Vulkan, a working systemd user session,
cgroup v2 and a Secret Service wallet. Windows browser features need WebView2.
Install and sign in to the provider CLI you want to use.

Acceptance: create a project/task, send and receive a message, restart and resume
that exact conversation, type/edit/copy/scroll in Terminal, and use Files/Git.
The source repository's docs/launch-readiness.md records outstanding launch gates.
"""
generated = {"README.txt": (readme.encode(), 0o644)}
if system == "windows":
    launcher_name = "launch-devmanager.cmd"
    launcher = '@echo off\r\nsetlocal\r\nset DEVMANAGER_DEBUG_HOST_PARENT_BOUND=1\r\n"%~dp0devmanager.exe" --dev-workspace "%~dp0."\r\n'
else:
    launcher_name = "launch-devmanager.sh"
    launcher = '\n'.join([
        '#!/bin/sh', 'set -eu',
        'candidate_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)',
        'export DEVMANAGER_DEBUG_HOST_PARENT_BOUND=1',
        'exec "$candidate_dir/devmanager" --dev-workspace "$candidate_dir"', '',
    ])
generated[launcher_name] = (launcher.encode(), 0o755)
manifest = {
    "schema": "devmanager.launch-candidate/v1",
    "commit": commit,
    "platform": f"{system}-{architecture}",
    "profile": "debug",
    "releaseApproved": False,
    "cargoLockSha256": digest(root / "Cargo.lock"),
    "files": [{"path": name, "bytes": source.stat().st_size, "sha256": digest(source)}
              for source, name, _ in files],
}
manifest["files"].extend({"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                         for name, (data, _) in generated.items())
destination = root / "dist" / "launch-candidates"
if not destination.resolve().is_relative_to(root):
    raise SystemExit("Candidate output escaped the checkout")
destination.mkdir(parents=True, exist_ok=True)
archive_name = f"devmanager-candidate-{system}-{architecture}-{commit[:12]}.zip"
with tempfile.TemporaryDirectory(prefix="assemble-", dir=destination) as staging:
    temporary = Path(staging) / archive_name
    with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
        for source, name, mode in files:
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            with source.open("rb") as incoming, archive.open(info, "w") as outgoing:
                while chunk := incoming.read(1024 * 1024):
                    outgoing.write(chunk)
        archive.writestr("candidate.json", json.dumps(manifest, indent=2) + "\n")
        for name, (data, mode) in generated.items():
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = (0o100000 | mode) << 16
            archive.writestr(info, data)
    with zipfile.ZipFile(temporary) as archive:
        if archive.testzip() is not None:
            raise SystemExit("Candidate archive verification failed")
        if set(archive.namelist()) != {name for _, name, _ in files} | set(generated) | {"candidate.json"}:
            raise SystemExit("Candidate archive escaped its allowlist")
        for entry in manifest["files"]:
            with archive.open(entry["path"]) as handle:
                actual = hashlib.file_digest(handle, "sha256").hexdigest()
            if actual != entry["sha256"]:
                raise SystemExit("Candidate input changed during assembly")
    final = destination / archive_name
    os.replace(temporary, final)
final.with_suffix(".zip.sha256").write_text(f"{digest(final)}  {archive_name}\n", encoding="utf-8")
print(final)
