# WezTerm for Android

Native Android client that attaches to a laptop's WezTerm multiplexer over
SSH through your own Tailscale network. Everything you type runs on the
laptop.

Current state: the app attaches to the laptop's mux server and renders its
existing windows and panes with WezTerm's own renderer. Typing into a pane
is not implemented yet. The connection has been exercised only against
test servers on the build machine, not over a real tailnet or on a phone.

## Connect

On the laptop, a WezTerm mux server must already be running (for example
`wezterm-mux-server --daemonize`, with the laptop GUI attached to it). The
app never starts one and never creates a session: a laptop without a
running mux server is reported as a failure, and a mux server without
sessions is shown as empty.

Open the app and enter:

- the laptop's Tailscale address (`100.x.y.z`, an `fd7a:115c:a1e0:` address
  or a `*.ts.net` name; anything else is refused, and a name that does not
  resolve to a Tailscale address fails without connecting anywhere),
- the SSH port if it is not 22, and the SSH user,
- optionally the absolute path of `wezterm` on the laptop, if it is not on
  the SSH session's `PATH`.

"Import SSH key…" opens the system file picker; the chosen OpenSSH or PEM
private key is copied into the app's private storage. Without a key the
app offers password login. The first connection shows the laptop's host key
fingerprint and asks whether to trust it; the answer is stored in the
app's own `known_hosts`. If the laptop later presents a different key, or
a key of another type, the app refuses to connect and keeps the stored key.
Connect is available once the app has finished starting.

The stored key, host trust and profile are excluded from Android backup
and device transfer. The key is stored as a file, not in the Android
Keystore.

Terminal windows live as long as the app process. Rotating, pressing Back
or Home, or switching apps only detaches the screen; reopening the app
shows the same window. When the laptop has more than one window, a
"Window 1 of 2" label appears in the top corner; tap it to pick the window
to show.

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
ci/android-sshmux-fixture.sh up "$(tailscale ip -4)"  # test sshd and mux servers on this machine, generated keys only
make android-test SERIAL=<serial> SUITE=sshmux        # connection screen, host trust, key import, attach, failures
ci/android-sshmux-fixture.sh down
```

The `sshmux` suite reaches the test servers through this machine's own
Tailscale address, which the kernel delivers over loopback. It tests the
app and the protocol; it does not show that traffic crosses a tailnet.

The launcher entry "WezTerm" opens the connection screen and logs
`WezTermSurface` lines; `org.wezterm.android/.DiagnosticActivity` still shows
the native initialization report (`WezTermDiag`). Debug builds show a
built-in diagnostic text grid instead of the connection screen when
launched with
`am start -n org.wezterm.android/.TerminalActivity --ez org.wezterm.android.DIAGNOSTIC_APPLET true`.

On the API 35 x86_64 emulator started with `-gpu swiftshader`, run the app
and the `surface` and `lifecycle` suites with
`WEZTERM_ANDROID_CONFIG_OVERRIDES='webgpu_preferred_adapter={name="Android Emulator OpenGL ES Translator (Google SwiftShader)",backend="Gl",device_type="Cpu"}'`;
its Vulkan implementation aborts the emulator otherwise (see `AGENTS.md`).
