#!/usr/bin/env python3
"""Fail when this checkout still owns a build, test or app process; never kill."""
import json
import os
from pathlib import Path
import sys

root = Path(__file__).resolve().parent.parent
target = (root / "target").resolve()
if sys.platform != "linux" or not target.is_relative_to(root) or target == root:
    raise SystemExit("Expected a Linux isolated checkout and target")
if Path(os.environ.get("CARGO_TARGET_DIR", target)).resolve() != target:
    raise SystemExit("Cargo target does not match this checkout")
remaining = []
for process in Path("/proc").iterdir():
    if not process.name.isdigit() or int(process.name) == os.getpid():
        continue
    try:
        executable = Path(os.readlink(process / "exe"))
        cwd = Path(os.readlink(process / "cwd"))
        name = (process / "comm").read_text().strip()
        owned = executable.is_relative_to(target)
        if name in ("cargo", "rustc", "rust-lld", "ld", "ld.lld"):
            arguments = (process / "cmdline").read_bytes().split(b"\0")
            owned |= cwd.is_relative_to(root) or any(os.fsencode(target) in arg for arg in arguments)
        if owned:
            remaining.append({"pid": int(process.name), "name": name, "executable": str(executable)})
    except (OSError, ProcessLookupError):
        continue
if remaining:
    print(json.dumps(remaining, indent=2))
    raise SystemExit("Owned build/test/app processes remain")
print("No owned harness, Cargo, compiler, linker or app process remains.")
