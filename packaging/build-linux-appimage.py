#!/usr/bin/env python3
"""Assemble the actual Linux client/host AppImage, then bind its signed identity.

Run in the Ubuntu 24.04 build environment after a locked release build. Only
allowlisted binaries/resources enter the AppDir; workspaces and profiles cannot
be copied by a recursive repository packaging step. Signing follows this script.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
MAGIC = b"DEVMANAGER-AI-V1"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run(command, **kwargs):
    print("Running", Path(command[0]).name, flush=True)
    return subprocess.run(command, check=True, **kwargs)


def tools(cache):
    pins = json.loads((ROOT / "packaging/linux-appimage-tools.json").read_text())
    cache.mkdir(parents=True, exist_ok=True)
    for filename, pin in pins.items():
        path = cache / filename
        if not path.exists():
            with urllib.request.urlopen(pin["url"], timeout=60) as response:
                with tempfile.NamedTemporaryFile(dir=cache, delete=False) as output:
                    temporary = Path(output.name)
                    shutil.copyfileobj(response, output)
            if digest(temporary) != pin["sha256"]:
                temporary.unlink()
                raise RuntimeError(f"Downloaded tool digest differs from reviewed pin: {filename}")
            temporary.replace(path)
        if digest(path) != pin["sha256"]:
            raise RuntimeError(f"Tool digest differs from reviewed pin: {filename}")
        path.chmod(0o755)
    return pins


def binary_metadata(path, role, version, env):
    result = run([str(path), "--package-identity"], env=env, capture_output=True, text=True, timeout=15)
    metadata = json.loads(result.stdout)
    expected = {
        "role": role, "version": version,
        "client_build": f"devmanager/{version}", "host_build": f"devmanager-host/{version}",
    }
    if any(metadata.get(key) != value for key, value in expected.items()):
        raise RuntimeError(f"Unexpected {role} binary identity: {metadata}")
    return metadata


def assemble(args):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise RuntimeError("This reviewed AppImage build targets Linux x86_64")
    release = args.binaries_dir.resolve(strict=True)
    destination = args.output.resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    cache = args.tools_cache.resolve()
    pins = tools(cache)
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    contract = json.loads((ROOT / "packaging/package-contract.json").read_text())
    env = os.environ.copy()
    env.update(APPIMAGE_EXTRACT_AND_RUN="1", DEPLOY_GTK_VERSION="3", ARCH="x86_64")
    env["PATH"] = str(cache) + os.pathsep + env["PATH"]
    # Verify both executable identities before dependency tools may modify ELF
    # rpaths. Hash the resulting packaged files after those modifications.
    metadata = [binary_metadata(release / name, role, version, env)
                for name, role in [("devmanager", "client"), ("devmanager-host", "host")]]
    if metadata[0]["protocol_major"] != metadata[1]["protocol_major"] or metadata[0]["protocol_minor"] != metadata[1]["protocol_minor"]:
        raise RuntimeError("Client/host protocol mismatch")
    if (metadata[0]["protocol_major"], metadata[0]["protocol_minor"]) != (contract["protocol"]["major"], contract["protocol"]["minor"]):
        raise RuntimeError("Binary protocol differs from reviewed package contract")
    with tempfile.TemporaryDirectory(prefix="devmanager-appimage-", dir=destination.parent) as temporary:
        appdir = Path(temporary) / "DevManager.AppDir"
        bindir = appdir / "usr/bin"
        bindir.mkdir(parents=True)
        for binary in ["devmanager", "devmanager-host"]:
            shutil.copy2(release / binary, bindir / binary)
        for resource in contract["resources"]:
            target = bindir / resource
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(ROOT / resource, target)
        desktop = appdir / "usr/share/applications/devmanager.desktop"
        desktop.parent.mkdir(parents=True)
        desktop.write_text("[Desktop Entry]\nType=Application\nName=DevManager\nExec=devmanager\nIcon=devmanager\nCategories=Development;\nTerminal=false\n")
        icon = appdir / "usr/share/icons/hicolor/256x256/apps/devmanager.png"
        icon.parent.mkdir(parents=True)
        shutil.copy2(ROOT / "packaging/icons/devmanager-256.png", icon)
        # WebKit helpers are processes, not ordinary link-time dependencies.
        # Their runtime path must stay inside this AppImage after installation.
        source_webkit = Path("/usr/lib/x86_64-linux-gnu/webkit2gtk-4.1")
        webkit = appdir / "usr/libexec/webkit2gtk-4.1"
        webkit.mkdir(parents=True)
        helpers = []
        for filename in ["WebKitWebProcess", "WebKitNetworkProcess", "WebKitGPUProcess"]:
            source = source_webkit / filename
            if not source.is_file():
                raise RuntimeError(f"Required WebKit helper missing: {source}")
            target = webkit / filename
            shutil.copy2(source, target)
            helpers.extend(["--executable", str(target)])
        shutil.copytree(source_webkit / "injected-bundle", webkit / "injected-bundle")
        # GIO loads TLS/proxy backends dynamically, and WebKit discovers media
        # decoders through GStreamer. Neither appears in the client's DT_NEEDED.
        modules = []
        for source, relative in [
            (Path("/usr/lib/x86_64-linux-gnu/gio/modules"), "usr/lib/gio/modules"),
            (Path("/usr/lib/x86_64-linux-gnu/gstreamer-1.0"), "usr/lib/gstreamer-1.0"),
        ]:
            target = appdir / relative
            shutil.copytree(source, target)
            for library in sorted(target.glob("*.so")):
                modules.extend(["--library", str(library)])
        scanner = appdir / "usr/libexec/gst-plugin-scanner"
        shutil.copy2("/usr/lib/x86_64-linux-gnu/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner", scanner)
        helpers.extend(["--executable", str(scanner)])
        for library in sorted((webkit / "injected-bundle").glob("*.so")):
            modules.extend(["--library", str(library)])
        command = [str(cache / "linuxdeploy-x86_64.AppImage"), "--appimage-extract-and-run",
                   "--appdir", str(appdir), "--executable", str(bindir / "devmanager"),
                   "--executable", str(bindir / "devmanager-host"), "--desktop-file", str(desktop),
                   "--icon-file", str(icon), *helpers, *modules, "--plugin", "gtk"]
        run(command, env=env)
        # Set each actual in-package ELF's search path, including dlopened
        # modules and the original helper locations (linuxdeploy may also copy
        # those helpers to usr/bin). Do not contaminate provider children with a
        # global LD_LIBRARY_PATH pointing into our private runtime.
        for candidate in appdir.rglob("*"):
            if candidate.is_symlink() or not candidate.is_file():
                continue
            with candidate.open("rb") as stream:
                if stream.read(4) != b"\x7fELF":
                    continue
            relative = os.path.relpath(appdir / "usr/lib", candidate.parent)
            run(["patchelf", "--set-rpath", "$ORIGIN:$ORIGIN/" + relative, str(candidate)],
                capture_output=True)
        # Distribution WebKit disables DEVELOPER_MODE, so WEBKIT_EXEC_PATH
        # is deliberately ignored. Relocate only the exact compiled helper
        # directory, retaining ELF string offsets. AppRun pins cwd to AppDir;
        # provider/Git children already select their host-authorized workspace.
        # This follows Tauri's WebKit AppImage relocation, with an exact prefix
        # instead of replacing every /usr occurrence in the library.
        original = str(source_webkit).encode()
        relative = b"./usr/libexec/webkit2gtk-4.1"
        relocated = b"./" + b"/" * (len(original) - len(relative)) + relative[2:]
        assert len(relocated) == len(original)
        changed = 0
        for library in (appdir / "usr/lib").glob("libwebkit2gtk-4.1.so*"):
            if library.is_symlink():
                continue
            data = library.read_bytes()
            changed += data.count(original)
            library.write_bytes(data.replace(original, relocated))
        if changed == 0:
            raise RuntimeError("Reviewed WebKit helper path was not present; revalidate runtime relocation")
        # linuxdeploy includes shared libraries; GTK's hook supplies relocatable
        # schemas, input modules, pixbuf loaders and themes. WebKit uses the
        # matching helper and injected-bundle directory explicitly.
        apprun = appdir / "AppRun"
        if apprun.is_symlink():
            apprun.unlink()
        apprun.write_text('''#!/bin/bash
set -e
export APPDIR="${APPDIR:-$(cd -- "$(dirname -- "$0")" && pwd -P)}"
# The pinned type-2 runtime consumes --appimage-extract-and-run without
# exporting its mode, and removes its extraction directory when its child
# exits. Preserve that mode across host launch/update exec, and place a
# detached host's extraction outside the client's cleanup directory.
# https://github.com/AppImage/type2-runtime/blob/main/src/runtime/runtime.c
if [[ "${APPDIR##*/}" =~ ^appimage_extracted_[0-9a-f]{32}$ ]]; then
    export APPIMAGE_EXTRACT_AND_RUN=1
    if [ "$#" -eq 0 ]; then
        runtime_cache="${XDG_CACHE_HOME:-$HOME/.cache}/com.userfirst.devmanager/appimage-runtime"
        runtime_temp="$runtime_cache/host-a"
        if [ "$(dirname -- "$APPDIR")" = "$runtime_temp" ]; then
            runtime_temp="$runtime_cache/host-b"
        fi
        umask 077
        mkdir -p -- "$runtime_temp"
        if [ -L "$runtime_temp" ] || [ ! -O "$runtime_temp" ]; then
            echo "DevManager runtime cache must be owned by the current user." >&2
            exit 1
        fi
        chmod 700 -- "$runtime_temp"
        export TMPDIR="$runtime_temp"
    fi
fi
for hook in "$APPDIR"/apprun-hooks/*.sh; do
    [ ! -f "$hook" ] || source "$hook"
done
cd -- "$APPDIR"
export WEBKIT_INJECTED_BUNDLE_PATH="$APPDIR/usr/libexec/webkit2gtk-4.1/injected-bundle"
export GIO_MODULE_DIR="$APPDIR/usr/lib/gio/modules"
export GST_PLUGIN_SYSTEM_PATH_1_0="$APPDIR/usr/lib/gstreamer-1.0"
export GST_PLUGIN_SCANNER="$APPDIR/usr/libexec/gst-plugin-scanner"
if [ "${1:-}" = "--devmanager-host" ]; then
    shift
    exec "$APPDIR/usr/bin/devmanager-host" "$@"
fi
exec "$APPDIR/usr/bin/devmanager" "$@"
''')
        apprun.chmod(0o755)
        documents = appdir / "usr/share/doc/devmanager"
        documents.mkdir(parents=True)
        for filename in ["LICENSE", "LICENSE.md", "THIRD_PARTY_NOTICES.md"]:
            source = ROOT / filename
            if source.is_file():
                shutil.copy2(source, documents / filename)
        (documents / "linux-package-tools.json").write_text(json.dumps(pins, indent=2) + "\n")
        (documents / "package-contract.json").write_text(json.dumps(contract, indent=2) + "\n")
        # Include distribution copyright notices for the bundled runtime. This
        # directory contains licensing text only, never system/user settings.
        copyrights = documents / "distribution-copyrights"
        copyrights.mkdir()
        for source in Path("/usr/share/doc").glob("*/copyright"):
            if source.is_file():
                shutil.copyfile(source, copyrights / (source.parent.name + ".txt"))
        packaged_env = env.copy()
        for binary, role in [("devmanager", "client"), ("devmanager-host", "host")]:
            binary_metadata(bindir / binary, role, version, packaged_env)
        unsigned = Path(temporary) / "DevManager.AppImage"
        run([str(cache / "appimagetool-x86_64.AppImage"), "--appimage-extract-and-run", "--no-appstream",
             "--runtime-file", str(cache / "runtime-x86_64"), str(appdir), str(unsigned)], env=env)
        manifest = {
            "schema": 1, "version": version, "platform": "linux-x86_64",
            "client_build": metadata[0]["client_build"], "host_build": metadata[0]["host_build"],
            "protocol_major": metadata[0]["protocol_major"], "protocol_minor": metadata[0]["protocol_minor"],
            "payload_sha256": digest(unsigned),
            "client_sha256": digest(bindir / "devmanager"), "host_sha256": digest(bindir / "devmanager-host"),
        }
        encoded = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
        assert 0 < len(encoded) <= 65536 and len(MAGIC) == 16
        with unsigned.open("ab") as stream:
            stream.write(encoded + len(encoded).to_bytes(8, "little") + MAGIC)
            stream.flush()
            os.fsync(stream.fileno())
        # Test the real runtime with the final trailer, without touching profiles
        # or starting a GUI/host. This also exercises the no-FUSE launch path.
        result = run([str(unsigned), "--appimage-extract-and-run", "--package-identity"],
                     env=env, capture_output=True, text=True, timeout=60)
        if json.loads(result.stdout) != metadata[0]:
            raise RuntimeError("Packaged AppImage reports a different client identity")
        result = run([str(unsigned), "--appimage-extract-and-run", "--devmanager-host", "--package-identity"],
                     env=env, capture_output=True, text=True, timeout=60)
        if json.loads(result.stdout) != metadata[1]:
            raise RuntimeError("Packaged AppImage reports a different host identity")
        unsigned.replace(destination)
        destination.chmod(0o755)
        destination.with_suffix(destination.suffix + ".identity.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Built {destination}: sha256:{digest(destination)}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--tools-cache", type=Path, required=True)
    assemble(parser.parse_args())
