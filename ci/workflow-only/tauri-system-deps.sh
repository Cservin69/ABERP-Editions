#!/usr/bin/env bash
# CI-runner setup, not a check: the Tauri 2 Ubuntu prerequisites (webkit2gtk,
# gtk-3, libsoup-3, librsvg, appindicator). Without the -dev packages the
# wry/tao build scripts fail at link. Listed in ci/gate-parity.sh.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  libsoup-3.0-dev \
  libgtk-3-dev
