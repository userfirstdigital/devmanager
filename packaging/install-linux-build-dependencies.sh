#!/usr/bin/env bash
# Ubuntu 24.04 is the reviewed Linux AppImage build/runtime baseline.
set -euo pipefail
apt-get update
apt-get install -y --no-install-recommends build-essential clang cmake pkg-config \
  libssl-dev libasound2-dev libfontconfig-dev libglib2.0-dev libwayland-dev \
  libx11-xcb-dev libxkbcommon-x11-dev libvulkan1 mesa-vulkan-drivers \
  libgtk-3-dev libwebkit2gtk-4.1-dev libzstd-dev glib-networking \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-libav \
  dbus-user-session lsof xvfb xauth ca-certificates curl git python3 file \
  patchelf squashfs-tools desktop-file-utils librsvg2-bin libfuse2t64 fonts-dejavu-core
