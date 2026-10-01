# WezTerm for Android

Native Android client that attaches to a laptop's WezTerm multiplexer over
SSH through your own Tailscale network. Everything you type runs on the
laptop.

Current state: the app renders WezTerm's own terminal window on a native
surface, using the bundled fonts plus the device's `/system/fonts` for CJK
and other scripts. Debug builds show a built-in diagnostic text grid;
nothing runs a shell and nothing connects anywhere yet.

Terminal windows live as long as the app process. Rotating, pressing Back
or Home, or switching apps only detaches the screen; reopening the app
shows the same window. When more than one window exists, a "Window 1 of 2"
label appears in the top corner; tap it to pick the window to show.

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
make android-test SERIAL=<serial> SUITE=native-load   # checks and rebuilds for the device ABI, then runs the suite
make android-test SERIAL=<serial> SUITE=surface       # renders, retires and resumes the terminal surface
make android-test SERIAL=<serial> SUITE=lifecycle     # rotation, Back, window selector, clipboard, engine failure
```

The launcher entry "WezTerm" opens the terminal surface and logs
`WezTermSurface` lines; `org.wezterm.android/.DiagnosticActivity` still shows
the native initialization report (`WezTermDiag`).

On the API 35 x86_64 emulator started with `-gpu swiftshader`, run the app
and the `surface` and `lifecycle` suites with
`WEZTERM_ANDROID_CONFIG_OVERRIDES='webgpu_preferred_adapter={name="Android Emulator OpenGL ES Translator (Google SwiftShader)",backend="Gl",device_type="Cpu"}'`;
its Vulkan implementation aborts the emulator otherwise (see `AGENTS.md`).
