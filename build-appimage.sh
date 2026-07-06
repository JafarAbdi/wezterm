#!/bin/bash
# Build a Linux AppImage of WezTerm in a container and drop it in ./out.
# The AppImage filename ends in the target arch (…-x86_64.AppImage etc).
#
#   ./build-appimage.sh                     # build for the host arch
#   PLATFORM=linux/arm64 ./build-appimage.sh  # cross-build (needs qemu binfmt)
#   BUILD_REASON=Schedule ./build-appimage.sh # nightly-style naming
#
# Building natively on the target machine (e.g. run this on the Raspberry Pi) is
# far faster than cross-building under emulation. Cross-builds need qemu-user
# registered with binfmt_misc on the host. With rootless podman, install the
# host package (persists across reboots, registers with the F flag):
#     sudo apt install -y qemu-user-static      # Debian/Ubuntu/Pop!_OS
#     sudo dnf install -y qemu-user-static      # Fedora
# Verify: podman run --rm --platform linux/arm64 ubuntu:22.04 uname -m  # -> aarch64
#
# Uses podman if present, otherwise docker.
set -euo pipefail
cd "$(dirname "$0")"

# Native (same-arch) builds work with either engine, so prefer rootless podman.
# Cross-arch builds (PLATFORM set) run under qemu emulation, which rootless
# podman's user namespace can't dispatch (emulated exec -> "exec format error"),
# so prefer rootful docker there. ENGINE overrides the choice either way.
if [ -n "${PLATFORM:-}" ]; then
    engine=${ENGINE:-$(command -v docker || command -v podman)}
else
    engine=${ENGINE:-$(command -v podman || command -v docker)}
fi
if [ -z "$engine" ]; then
    echo "no container engine found (install podman or docker)" >&2
    exit 1
fi
tag_name=${TAG_NAME:-$(ci/tag-name.sh)}

platform_args=()
if [ -n "${PLATFORM:-}" ]; then
    platform_args=(--platform "$PLATFORM")
fi

exec "$engine" build \
    --target export \
    --output out \
    "${platform_args[@]}" \
    --build-arg TAG_NAME="$tag_name" \
    --build-arg BUILD_REASON="${BUILD_REASON:-}" \
    .
