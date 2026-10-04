# WezTerm for Android

Native Android client that attaches to a laptop's WezTerm multiplexer over
SSH through your own Tailscale network. Everything you type runs on the
laptop.

The app renders existing mux panes with WezTerm's native renderer.
Emulator fixtures verify attachment and input. Physical ARM64, a separate
private Tailscale endpoint, human input, and hardware performance remain
unverified. A locally signed APK is release preparation, not phone acceptance.

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

While connecting, Cancel (and "Stop connecting" in a trust or password
dialog) stops the attempt; nothing it started stays behind, and the
laptop's sessions are not touched. A cancel while the laptop's name is
still being looked up shows "Cancelling…" until the system's lookup
returns, and nothing is dialed after it. The "…" key next to Paste disconnects
from the laptop after asking. A lost connection shows "connection lost".
In both cases the laptop's sessions keep running, and Reconnect shows them
again; nothing typed while disconnected is sent. Nothing reconnects by
itself: not after a lost connection, not after the laptop's last pane
exits (the app then shows the laptop as empty), and not when the app is
started again after Android stopped it, which needs Connect.

Terminal windows live as long as the app process. Rotating, pressing Back
or Home, or switching apps only detaches the screen; reopening the app
shows the same window. When the laptop has more than one window, a
"Window 1 of 2" label appears in the top corner; tap it to pick the window
to show. If the laptop closes the window the phone shows, the phone shows
its first window.

## Type

Tap the terminal to open the keyboard. What you type goes to the shown
laptop pane once the keyboard commits it; text still being composed
(for example pinyin before you pick the characters, or a word the
keyboard reopened for correction) is shown in the terminal but not sent.
Backspace deletes on the laptop. If the laptop switches to another pane
while you type, even if it switches back, or the pane is resized, a
keyboard correction of text you already sent is dropped, never applied.

The row above the keyboard has Esc, Ctrl and Alt (each applies to the
next key you type), Tab, the arrow keys and Paste. A hardware keyboard
works too, with its layout's AltGr characters and accent keys; its Esc
goes to the laptop even while the on-screen keyboard is open, and
Ctrl+Shift+V or Shift+Insert pastes.

Tapping a pane focuses it on the laptop too. A focus change made on the
laptop is only shown, not taken over: a `wezterm cli` command on the
laptop without `--pane-id`, run outside a pane (no `WEZTERM_PANE`), may
act on the pane you last tapped rather than the one the phone shows.
Pass `--pane-id`.

Drag to scroll. Long-press and drag to select; lifting the finger copies
the selection to the clipboard. Programs that use the mouse, such as vim
with `set mouse=a`, get taps and selections as mouse clicks instead.

The terminal's size is the size of the laptop pane. Opening the keyboard
or rotating the phone resizes that pane on the laptop, for every client
attached to it, including the laptop's own window. While the phone shows
a pane, the pane keeps the phone's size: if another client resizes it,
the phone resizes it back. While the phone shows nothing (another app in
front, the screen off), the laptop's resizes stay; the pane takes the
phone's size again when the phone shows it. Two phones showing the same
pane at different sizes keep resizing it back and forth.

## Build

Install `rustup`, `curl`, `uv`, and the Android SDK components pinned in
`gradle.properties`. Use a Linux x86_64 build host.

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
make android-install SERIAL=<serial>
make android-test SERIAL=<serial> SUITE=native-load   # checks and rebuilds for the device ABI, then runs the suite
make android-test SERIAL=<serial> SUITE=surface       # renders, retires and resumes the terminal surface
make android-test SERIAL=<serial> SUITE=lifecycle     # rotation, Back, window selector, clipboard, engine failure
ci/android-sshmux-fixture.sh up <owned-host-address> # generated fixture keys and sessions only
make android-test SERIAL=<serial> SUITE=sshmux        # connection screen, host trust, key import, attach, failures
make android-test SERIAL=<serial> SUITE=input         # typing, keys, paste, selection, resize; needs a fresh `up`
make android-test SERIAL=<serial> SUITE=reconnect     # cancel, lost connection, reconnect, force-stop, last-pane exit, repeated rounds; needs a fresh `up`
ci/android-sshmux-fixture.sh down
```

The `sshmux`, `input` and `reconnect` suites reach the test servers through this
machine's own Tailscale address, which the kernel delivers over loopback.
They test the app and the protocol; they do not show that traffic crosses
a tailnet.

The launcher entry "WezTerm" opens the connection screen.
Only debug APKs contain `org.wezterm.android/.DiagnosticActivity`, which
shows the native initialization report. Debug builds show a
built-in diagnostic text grid instead of the connection screen when
launched with
`am start -n org.wezterm.android/.TerminalActivity --ez org.wezterm.android.DIAGNOSTIC_APPLET true`.

On the API 35 x86_64 emulator started with `-gpu swiftshader`, run the app
and the `surface` and `lifecycle` suites with
`WEZTERM_ANDROID_CONFIG_OVERRIDES='webgpu_preferred_adapter={name="Android Emulator OpenGL ES Translator (Google SwiftShader)",backend="Gl",device_type="Cpu"}'`;
its Vulkan implementation aborts the emulator otherwise.

## Build a signed release

Release signing has no unsigned or debug-key fallback. Put your keystore and
password files outside the repository. Set file permissions to `0600` and
keep the containing directory private. Passwords belong in files, never in
command arguments or logs.

```sh
export WEZTERM_ANDROID_KEYSTORE=/private/delivery.p12
export WEZTERM_ANDROID_KEY_ALIAS=delivery
export WEZTERM_ANDROID_STORE_PASSWORD_FILE=/private/store-password
export WEZTERM_ANDROID_KEY_PASSWORD_FILE=/private/key-password
make android-release
make android-release-inspect
make android-inspect-selftest VARIANT=release
```

Install `android/app/build/outputs/apk/release/app-arm64-v8a-release.apk` on
an explicitly selected ARM64 device with
`make android-install SERIAL=<serial> VARIANT=release`.
Android refuses an update signed by a different key. Replacing a debug
installation requires removing it first, which erases its private app data.
Do not do that to preserve an existing profile or trust store.

Keep the keystore and password files in secure, recoverable custody for
updates. Share the APK, certificate digest, notices, and symbol archive,
not the key or passwords. A newly minted local test key does not confer
operator release authority.

The release receipt directory is `target/android-release/receipts`.
It records source hashes, tool inputs, certificate identity, and artifact
checksums. Unstripped crash symbols are in `target/android-release/symbols`.
The APK contains dependency and font notices under `assets/notices`.
Preserve the symbols and the R8 mapping in
`android/app/build/outputs/mapping/release` with the matching APK.
Recorded inputs do not establish byte-for-byte reproducibility across hosts.

```sh
make android-test SERIAL=<owned-emulator> SUITE=release-load
```

This suite loads the signed native library, launches the production profile
screen, refuses a non-tailnet address, and opens the system document picker.
It does not connect to a laptop or pass physical-phone acceptance.
Never substitute it for the full phone terminal, trust, input, lifecycle,
reconnect, private-route, and human usability checks.

Default logging excludes input, composition, paste, decoded PDU bodies,
and clipboard text. `debug_key_events` is an unsafe explicit opt-in that
can log keys. It remains off in tests and release.

## Release limits

DNS cancellation waits for the system resolver. A fatal GUI failure can
park its thread and strand a mux domain after transport workers stop.
API35 graphics descriptors grew in lifecycle testing. Rare failed-font
cases can overlap regional-indicator glyphs. Do not interpret emulator
results as hardware-driver or private-network proof.
