#!/usr/bin/env python3
"""Restore reviewed WASM inputs and bind Cargo to this isolated CI checkout."""
import hashlib
import json
import os
from pathlib import Path
import shutil

root = Path(__file__).resolve().parent.parent
target = (root / "target").resolve()
if not target.is_relative_to(root) or target == root:
    raise SystemExit("Cargo target escaped the isolated checkout")
configured = os.environ.get("CARGO_TARGET_DIR")
if configured and Path(configured).resolve() != target:
    raise SystemExit("Existing Cargo target does not match the isolated checkout")
target.mkdir(parents=True, exist_ok=True)
source = root / "web/bundle/assets/wasm"
manifest_path = source / "connect_crypto.manifest.json"
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
expected = {"connect_crypto.js", "connect_crypto_bg.wasm", "connect_crypto.d.ts", "connect_crypto_bg.wasm.d.ts"}
if {entry["path"] for entry in manifest["files"]} != expected or len(manifest["files"]) != len(expected):
    raise SystemExit("Unexpected Connect artifact allowlist")
for entry in manifest["files"]:
    data = (source / entry["path"]).read_bytes()
    if len(data) != entry["bytes"] or hashlib.sha256(data).hexdigest() != entry["sha256"]:
        raise SystemExit(f"Reviewed Connect artifact is corrupt: {entry['path']}")
destination = root / "web/src/connect/wasm"
destination.mkdir(parents=True, exist_ok=True)
for name in sorted(expected | {manifest_path.name}):
    shutil.copy2(source / name, destination / name)
print(f"Isolated Cargo target: {target}")
print("Restored verified Connect WASM fingerprint inputs")
if output := os.environ.get("GITHUB_ENV"):
    with Path(output).open("a", encoding="utf-8") as handle:
        handle.write(f"CARGO_TARGET_DIR={target}\n")
