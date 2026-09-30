# Android port: ownership and build constraints

Read `README.md` for installation. This file is for agents and covers
architecture boundaries and the build policy.

## What this is

A native Android client for the laptop's existing WezTerm SSHMUX over the
user's private Tailscale network. All commands run on the laptop. The APK
never starts a local shell, web gateway, public listener, VPN or replacement
terminal engine.

## Ownership

| Layer | Owns | Never owns |
|---|---|---|
| `wezterm-gui` (lib) | `Mux`, `ClientDomain`, `TermWindow`, fonts, glyph cache, renderer | Android lifecycle |
| `window::os::android` | `Connection`/`Window` contracts, later the native-window lease | pane state |
| `wezterm-android` (cdylib) | JNI exports, app-private path publication, initialization report | a second terminal model |
| Kotlin (`android/app`) | Activities, `SurfaceView`, IME, dialogs, document picker, clipboard | pane cache, wire protocol |

Unsafe code lives only in `wezterm-android/src/ffi.rs`; other modules of that
crate `forbid(unsafe_code)`. JNI exports upgrade `EnvUnowned` with `with_env`
so panics become `RuntimeException`s instead of unwinding into ART.

The process environment is never mutated on Android. `config::AndroidPaths`
is published once through `config::set_android_paths` before any path static
is read; `HOME_DIR`, `CONFIG_DIRS`, `DATA_DIR`, `CACHE_DIR` and `RUNTIME_DIR`
resolve from it, and the `WEZTERM_CONFIG_*` environment publication is
compiled out. SSH still reads `~/.ssh` through `dirs_next::home_dir()`; a
later stage routes it through explicit config instead.

## Stage status

ANDROID-01 (this checkout): the full GUI closure cross-builds, links and
initializes. `DiagnosticActivity` shows the report. `window::os::android`
returns `Unsupported` for every windowing operation. No terminal renders and
no SSHMUX connection exists yet.

## Build policy

Single runner: `ci/android.sh`, wrapped by `make android-*`. Policy values
(`minSdk`, `compileSdk`, `targetSdk`, NDK, build-tools, Rust toolchain,
packaged ABIs) live once in `android/gradle.properties` and are read by both
Gradle and the script.

- Toolchain table (ABI, Rust target, ELF machine) is in `ci/android.sh`.
  Shipping ABI is `arm64-v8a`; `x86_64` is for the emulator.
- Gradle's `preBuild` depends on `cargoNative_<abi>` for every ABI in
  `wezterm.abis`, which runs `ci/android.sh native <abi>` from the repo root.
  `build` and `test` therefore always package the native library of the
  current source for every ABI; `native` alone never runs Gradle, so the
  path is not recursive. Do not narrow the ABI split per invocation: Gradle
  deletes the APKs of ABIs missing from the split, which breaks `inspect`.
- `minSdk=24`. Rust std needs 21, bionic `openpty()` (linked through
  `portable-pty`) needs 23, `libvulkan.so` for wgpu ships from 24. The
  `inspect` gate lists undefined symbols the API-24 sysroot does not export.
- Pinned inputs: Rust `wezterm.rustToolchain` (exported as
  `RUSTUP_TOOLCHAIN` for every cargo call of the runner; the desktop build
  keeps its own toolchain choice), cargo-ndk 4.1.2, Temurin JDK 17.0.20.1+1
  (sha256 in the script), Gradle 8.10.2 (sha256 in the wrapper), AGP 8.7.3,
  Kotlin 2.0.21, NDK 28.0.13004108, build-tools 35.0.0, platform 35.
  `provision` installs everything under `$WEZTERM_ANDROID_TOOLCHAIN` and
  never touches system Java.
- Cargo runs `--locked`. Host `pkg-config` is refused for target builds
  (`PKG_CONFIG_ALLOW_CROSS` stays unset); every native library is vendored
  and compiled with the NDK clang that cargo-ndk configures.
- `wezterm-android` depends on `wezterm-gui` with `default-features = false`
  plus `vendored-fonts`; there is no Wayland/X11/Fontconfig/D-Bus on Android.
- `inspect` is a gate, not a report: it fails nonzero unless both ABIs have a
  jniLibs library, unstripped symbols and a signed APK; each library is
  ELF64/DYN of the expected machine, 16 KiB LOAD aligned, has no
  RPATH/RUNPATH, links only `libandroid libc libdl liblog libm`, carries the
  pinned rustc and NDK identity, exports the required JNI symbols and has
  zero strong symbols unresolved against the API-24 sysroot; each APK
  declares exactly that ABI, contains exactly that library with the same
  `.text` as jniLibs, verifies with APK Signature Scheme v2, passes
  `zipalign -P 16`, and all APKs share one signing certificate.
  `inspect-selftest` proves the gate rejects missing, foreign, truncated and
  stale artifacts using symlinked fixture trees under
  `target/android-inspect/selftest/`.
- Debug APKs are signed with the Android debug keystore. Unstripped
  libraries are kept in `target/android-symbols/<abi>/`.
- Preserve the workspace's editions, nightly `rustfmt`, and licenses. Only
  `wezterm-android` opts into `[workspace.lints]`.
