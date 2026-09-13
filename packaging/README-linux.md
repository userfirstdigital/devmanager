# Linux AppImage

DevManager ships one x86-64 AppImage containing the desktop client, its matching
background host, and the GTK/WebKit runtime. The baseline is Ubuntu 24.04 or a
compatible distribution with glibc 2.39 or newer. The desktop uses X11, including
XWayland on a Wayland desktop. Provider CLIs and Git run from the user's machine.

Download `DevManager_<version>_x86_64.AppImage`, make it executable, and open it:

```sh
chmod +x DevManager_0.4.2_x86_64.AppImage
./DevManager_0.4.2_x86_64.AppImage
```

Keep the AppImage in a directory owned by your user, such as `~/Applications`,
so Settings → Updates can replace it. Updating verifies the signed package,
replaces the client and host together, and restarts DevManager. Your projects,
settings, and conversations remain in the normal user profile. Closing the
window keeps the background host available; reopening reconnects to it.

If the machine cannot mount AppImages with FUSE, use:

```sh
./DevManager_0.4.2_x86_64.AppImage --appimage-extract-and-run
```

## Building

The release workflow builds on Ubuntu 24.04, runs the serial Linux verification
suite, assembles the AppImage, and signs it using the protected release key.
`linux-build.Dockerfile` provides the same native build dependencies locally.
Run `install-linux-build-dependencies.sh` as root in a disposable Ubuntu build
environment, then build both `devmanager` and `devmanager-host` with Cargo.

```sh
python3 packaging/build-linux-appimage.py \
  --binaries-dir target/release \
  --output dist/linux/DevManager_0.4.2_x86_64.AppImage
```

The builder checks both binaries' version and protocol identities, uses tools
pinned by SHA-256 in `linux-appimage-tools.json`, bundles the required native
helpers and modules, and executes both packaged identities before succeeding.
The AppImage trailer binds the bundled client and host hashes to its payload;
the release signature covers the complete artifact, including that trailer.
A local build needs the configured public update key and endpoint to offer
updates. Private signing keys are used only by the signing step.
