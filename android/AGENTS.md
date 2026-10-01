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
| `window::os::android` | `Connection`/`Window` contracts, the GUI-thread message loop, the logical windows and which one is bound, the surface slot state machine, native-window leases, the request queue to Kotlin | pane state |
| `wezterm-android` (cdylib) | JNI exports, GUI thread lifecycle, the platform event mailbox (`EngineGate`), app-private path publication, initialization report | a second terminal model |
| Kotlin (`android/app`) | Activities, `SurfaceView`, IME, dialogs, the window selector, document picker, clipboard | pane cache, window list copy, wire protocol |

Unsafe code lives in `wezterm-android/src/ffi.rs` (JNI exports and
`ANativeWindow_fromSurface`) and in one documented block of
`window/src/os/android/window.rs` (borrowing the raw `ANativeWindow` handle
for wgpu). Every other Android module `forbid(unsafe_code)`. JNI exports
upgrade `EnvUnowned` with `with_env` so panics become `RuntimeException`s
instead of unwinding into ART.

## Surface lifecycle

Kotlin tags every `SurfaceHolder` callback with a process-wide increasing
generation. `window::surface::SurfaceState` (pure, host-tested) maps
`Created`/`Changed`/`Destroyed` to `Absent`/`Unsized`/`Present` and to the
effects the GUI thread applies: `WindowEvent::SurfaceAvailable` creates the
wgpu state (deferred until a nonzero size exists), `SurfaceLost` drops it,
stale generations are ignored and counted. A `NativeWindowLease` owns the
acquired `ANativeWindow`; `WebGpuState` holds a `SurfaceLease` clone, so the
window is released only after every GPU reference. `surfaceDestroyed`
blocks on that release; the GUI thread never calls into Java, so the wait
cannot deadlock. Surface loss never reaches `CloseRequested`, `Destroyed`
or the frontend's `kill_window` path. The GUI thread sleeps in `poll()` on
the promise spawn-queue pipe; every wake-up is a queued task (surface
event, mux notification, timer, invalidation) and painting happens once
per drained queue.

`wezterm_android::terminal` owns the single GUI thread per process.
`wezterm_android::engine::EngineGate` (host-tested) holds the engine phase
and every `PlatformEvent` Kotlin posted that the GUI thread has not applied:
surface callbacks, a window selection, a clipboard answer. Events queue
while the engine starts and while it runs; a running GUI thread is woken
through the spawn queue and takes one event at a time, in arrival order, so
a destroy can never overtake the creation it belongs to and no event with a
lease or a waiter ever sits in a spawn-queue closure. The surface slot
keeps the newest generation it held or saw destroyed, so a creation replayed
after its destroy is stale.

The engine ends through one ordered shutdown (`EngineGate::shut`), whether
bootstrap failed, the message loop returned or the GUI thread panicked:
stop accepting events; close the request queue, which fails every
unanswered clipboard read; on the GUI thread dispatch `SurfaceLost` to the
bound window and release the lease the slot holds (`Connection::retire`);
release every queued lease; acknowledge every queued destroy. No task
queued for the GUI thread runs after a failure. `surfaceDestroyed` returns
`false` instead of an acknowledgment only when the engine ended without
confirming the release; even then it returns after the shutdown finished.
A window whose handler panics during the shutdown keeps its GPU state and
its lease clone; that is logged and reported as not released. After a
failure the GUI thread parks forever: its thread-local object graph is not
safe to destroy (a window destructor panicked at thread exit and aborted
the process). Nothing in the shutdown closes a window or reaches the mux.
A GUI thread that hangs without ending still blocks `surfaceDestroyed`.

Logical windows outlive surfaces and Activities. `Connection` keeps every
window; the first one binds to the surface slot and `select_window`
rebinds. Only the bound window receives `SurfaceAvailable`, `Resized` and
repaints; rebinding dispatches `SurfaceLost` to the old window before the
new one creates GPU state on the same native window. A deliberate close of
the bound window binds the lowest remaining id. The monitor snapshot lists
every window (`windows`: id and title) for the selector; a surfaceless
window computes its title at creation and on mux title alerts.
`TerminalActivity` shows the list in a dialog and posts the chosen id; it
keeps no copy. Back calls `moveTaskToBack`; the Activity is `singleTask`
because one surface slot exists per process.

The GUI thread never calls into Java. What it needs from the platform
(`PlatformRequest`: clipboard read, clipboard write, window list changed)
it queues in `window::os::android::platform_requests()`; the Kotlin thread
`wezterm-requests` blocks in `nativeNextRequest` and posts each request to
the UI thread (`PlatformRequests.kt`). A clipboard read is a `Promise` the
request queue owns from the `get_clipboard` call until the `ClipboardText`
event answers it or the queue closes, so no read depends on a GUI-thread
task running, and a UI thread blocked in `surfaceDestroyed` delays the
answer while nothing waits on it.

`nativeSurfaceStatus` exposes the counters (`revision`, `generation`,
`frames_presented`, `stale_events`, `retire_acks`, `live_leases`,
`windows`, `closed_windows`, `clipboard_requests`, `clipboard_responses`,
`loop_wakeups`); `nativeAwaitSurfaceChange`, `nativeAwaitSurfaceFrames` and
`nativeAwaitSurfaceState` are condition waits on them. A surface destroy
publishes `absent` before it releases the lease; the shutdown releases the
lease first and publishes `absent` after. The thread inside
`surfaceDestroyed` sees both when it returns; other observers wait for
`absent` and `live_leases == 0`.

The process environment is never mutated on Android. `config::AndroidPaths`
is published once through `config::set_android_paths` before any path static
is read; `HOME_DIR`, `CONFIG_DIRS`, `DATA_DIR`, `CACHE_DIR` and `RUNTIME_DIR`
resolve from it, and the `WEZTERM_CONFIG_*` environment publication is
compiled out. SSH still reads `~/.ssh` through `dirs_next::home_dir()`; a
later stage routes it through explicit config instead.

## Stage status

ANDROID-01: the full GUI closure cross-builds, links and initializes;
`DiagnosticActivity` shows the report.

ANDROID-02: `TerminalActivity` binds one logical window to its
`SurfaceView` and `TermWindow` renders through wgpu into it. Debug
builds open a termwiz diagnostic applet (`wezterm_gui::android::diagnostic`,
no shell, no domain) whose literal grid is the device evidence. Mux starts
with no default domain; no SSHMUX connection exists yet. `front_end`
defaults to `WebGpu` on Android; glium `enable_opengl` stays `Unsupported`.
Glyph fallback: the bundled faces cover Latin, symbols and emoji; on Android
`font_dirs` defaults to `/system/fonts` (the directory `/system/etc/fonts.xml`
names) and `search_font_dirs_for_fallback` to true, so CJK and other scripts
resolve through the existing `FontDatabase` coverage lookup without a font
service or a bundled CJK face. Fallback faces are not ordered by locale: a
Han character may come from any regional Noto Sans CJK face on the device.
GPU failures on the bound window (`WebGpuState`/`RenderState` creation, a
draw) are counted in `wezterm_gui::renderfault` and reported in
`nativeSurfaceStatus` (`render`); the window stays surfaceless or skips the
frame, and the next surface or invalidation renders again. Debug builds arm
a one-shot failure through `nativeDiagnosticFault(stage)` and wait on the
counters with `nativeAwaitRenderFailures`.

ANDROID-03 (this checkout): the ordered shutdown, the platform event
mailbox, logical windows with a Kotlin selector, the request queue and the
clipboard described above. Debug builds drive them through `nativeDiagnosticGui`
(`open-window`, `paste`, `panic-on-queued-destroy`,
`panic-with-clipboard-read`, `panic-in-surface-lost`). No SSHMUX connection
exists yet; input other than the debug paste is ANDROID-05.

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
  stale artifacts, symbols without DWARF, DWARF left in jniLibs, and
  symbols whose `.text` or defined symbols differ from the shipped
  library, using symlinked fixture trees under
  `target/android-inspect/selftest/`.
- `test <serial> <suite>` runs `check` for the device ABI first. Suites:
  `native-load` (`NativeLoadTest`), `surface` (`SurfaceTest`: frames,
  stale generations, zero size, rotation resize, ordered retire/resume,
  and recovery from an injected GPU creation failure and draw failure) and
  `lifecycle` (`LifecycleTest`: rotation during engine start, backgrounding
  with a repaint pending, duplicate and late surface events, a clipboard
  read racing surface destruction, Back and reopen, the window selector,
  idle redraws and owner counts over resume cycles; then each
  `EngineFailureTest` method in its own process: a GUI-thread panic with a
  destroy queued, a panic right after a clipboard read started, a bootstrap
  failure before a `Connection` exists, and a `SurfaceLost` handler panic
  on a surface destroy and inside the shutdown). The entries of a suite
  (class or `class#method`) run one process each because the engine starts
  once per process; reports are copied to
  `android/app/build/outputs/androidTest-results/<suite>/<entry>.xml`.
  `WEZTERM_ANDROID_CONFIG_OVERRIDES` (`key=value` lines, debug builds only)
  reaches `TerminalActivity` as an intent extra; the same extra
  (`org.wezterm.android.CONFIG_OVERRIDES`) works with `am start --es`.
- The API 35 x86_64 emulator with `-gpu swiftshader` aborts the whole
  emulator process when wgpu creates a Vulkan pipeline (its SwiftShader
  rejects naga's `OpSource WGSL`). Pin the GL adapter there:
  `webgpu_preferred_adapter={name="Android Emulator OpenGL ES Translator (Google SwiftShader)",backend="Gl",device_type="Cpu"}`.
  On Android only, `WebGpuState` instantiates just the preferred backend (a
  Vulkan surface would connect the native window and block EGL); desktop
  keeps every backend. Device creation retries with the WebGL2 limit
  profile for GLES 3.0 adapters.
- Debug APKs are signed with the Android debug keystore. `native` keeps
  the unstripped library in `target/android-symbols/<abi>/` and writes a
  `llvm-strip --strip-debug` copy to jniLibs: same `.text` and symbol
  tables, no DWARF. Gradle copies its jniLibs input several times per ABI,
  and the unstripped file is about 600 MiB. `inspect` checks all three
  properties.
- `wezterm-version/build.rs` reruns when `HEAD` of this worktree, the
  branch ref in the common git dir or `packed-refs` changes, so artifacts
  built after a commit embed that commit. `.tag` still wins when present.
- Preserve the workspace's editions, nightly `rustfmt`, and licenses. Only
  `wezterm-android` opts into `[workspace.lints]`.
