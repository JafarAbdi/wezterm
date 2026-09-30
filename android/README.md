# WezTerm for Android

Native Android client that attaches to a laptop's WezTerm multiplexer over
SSH through your own Tailscale network. Everything you type runs on the
laptop.

Current state: the app only shows a native initialization diagnostic. It does
not render a terminal or connect anywhere yet.

## Build

Requirements: Android SDK with platform 35, build-tools 35.0.0 and NDK
28.0.13004108; `rustup`; `curl`.

```sh
export ANDROID_HOME=/path/to/android-sdk
make android-provision              # pinned Rust toolchain and targets, cargo-ndk, JDK 17 into ~/.local/share/wezterm-android-toolchain
make android-build                  # every ABI in android/gradle.properties; Gradle rebuilds the native libraries first
make android-inspect                # fails unless both APKs and libraries pass every check
make android-inspect-selftest       # proves the inspect gate rejects bad artifacts
```

APKs land in `android/app/build/outputs/apk/debug/app-<abi>-debug.apk`.

## Install and test

```sh
adb devices
make android-install SERIAL=<serial>
make android-test SERIAL=<serial> SUITE=native-load   # rebuilds for the device ABI, then runs the suite
```

The launcher entry "WezTerm" opens the diagnostic screen and prints a
`WezTermDiag` line to logcat.
