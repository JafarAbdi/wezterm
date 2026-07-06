# syntax=docker/dockerfile:1
#
# Build a Linux AppImage of WezTerm inside a container.
#
#   podman build --target export -o out --build-arg TAG_NAME="$(ci/tag-name.sh)" .
#
# The AppImage lands in ./out. See build-appimage.sh for the convenience wrapper.
#
# ubuntu:22.04 matches upstream CI and gives the oldest supported glibc (2.35),
# so the resulting AppImage runs on that and any newer distro.

FROM ubuntu:22.04 AS builder

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH \
    CARGO_INCREMENTAL=0 \
    CI=yes \
    # linuxdeploy and appimagetool are AppImages; no FUSE in a container,
    # so tell them to self-extract instead of mounting a squashfs.
    APPIMAGE_EXTRACT_AND_RUN=1

# Base tooling. Build/system deps come from ./get-deps below.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git file \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain (apt's is too old). Installed before get-deps, which fails if
# cargo is not already on PATH.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable

# System build dependencies, sourced from the repo's own script so this
# stays in sync with upstream instead of duplicating the apt list. get-deps
# runs ./ci/check-rust-version.sh relative to its cwd, so both are staged with
# that layout — copied on their own (not the whole tree) to keep this layer
# cached across source changes.
COPY --link get-deps /opt/deps/get-deps
COPY --link ci/check-rust-version.sh /opt/deps/ci/check-rust-version.sh
RUN cd /opt/deps && apt-get update && sh get-deps && rm -rf /var/lib/apt/lists/*

# linuxdeploy is pulled from the moving "continuous" tag; cache it as its own
# layer so rebuilds skip the download (appimage.sh reuses /tmp/linuxdeploy).
# The asset name matches `uname -m` (x86_64/aarch64/armhf/i386), which under
# emulation reflects the target arch, so this works for native and cross builds.
RUN set -eux; \
    case "$(uname -m)" in \
        x86_64)        ld_arch=x86_64 ;; \
        aarch64)       ld_arch=aarch64 ;; \
        armv7l|armv6l) ld_arch=armhf ;; \
        i686|i386)     ld_arch=i386 ;; \
        *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fL "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${ld_arch}.AppImage" \
        -o /tmp/linuxdeploy; \
    chmod +x /tmp/linuxdeploy; \
    # AppImages stamp "AI\x02" at ELF offset 8, which stops binfmt_misc from
    # matching the ELF and dispatching to qemu under emulation ("exec format
    # error"). Zero those bytes so cross-arch builds can run the tool; the ELF
    # stays valid and native builds are unaffected. Maintainer-endorsed fix:
    # https://github.com/AppImage/AppImageKit/issues/965
    # https://github.com/AppImage/AppImageKit/issues/1056
    dd if=/dev/zero of=/tmp/linuxdeploy bs=1 seek=8 count=3 conv=notrunc status=none

WORKDIR /build
COPY --link . .

# TAG_NAME drives both the embedded `wezterm --version` (via .tag, read by
# wezterm-version/build.rs) and the AppImage filename (via appimage.sh). Writing
# .tag means the build needs no .git, so it is excluded from the context.
ARG TAG_NAME=dev
ARG BUILD_REASON
# TARGETARCH (amd64/arm64/arm/...) is auto-populated by the builder; it scopes
# the caches per-arch so cross-builds on one host don't mix object files.
ARG TARGETARCH
RUN echo "$TAG_NAME" > .tag

# Compile and package in a single RUN: the target/ cache mount only exists for
# the duration of a RUN, and appimage.sh reads target/release/*. The resulting
# *.AppImage is written to the workdir (not the mount), so it survives. The arch
# is appended to the filename so artifacts from different arches don't collide.
RUN --mount=type=cache,id=wezterm-target-${TARGETARCH},target=/build/target \
    --mount=type=cache,id=wezterm-registry-${TARGETARCH},target=/root/.cargo/registry \
    --mount=type=cache,id=wezterm-git-${TARGETARCH},target=/root/.cargo/git \
    cargo build --release \
        -p wezterm \
        -p wezterm-gui \
        -p wezterm-mux-server \
        -p strip-ansi-escapes \
    && TAG_NAME="$TAG_NAME" BUILD_REASON="$BUILD_REASON" bash ci/appimage.sh \
    && arch="$(uname -m)" \
    && for f in *.AppImage; do mv -- "$f" "${f%.AppImage}-${arch}.AppImage"; done

# Export stage: `-o out` extracts just the artifact to the host.
FROM scratch AS export
COPY --from=builder /build/*.AppImage /
