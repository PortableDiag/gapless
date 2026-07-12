#!/usr/bin/env bash
# Build + runtime dependencies for the gapless player on Ubuntu 24.04.
# Needs root, so run it yourself: ./scripts/setup-deps.sh
set -euo pipefail

sudo apt update
sudo apt install -y \
  build-essential pkg-config \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libgtk-4-dev libadwaita-1-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav gstreamer1.0-pipewire

echo
echo "Dependencies installed. Now: cargo run --release"
