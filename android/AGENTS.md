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
| `wezterm-android` (cdylib) | JNI exports, GUI thread lifecycle, the platform event mailbox (`EngineGate`), the translation of phone input into window events (`input`), app-private path publication, initialization report, the validated `Profile`, the connection phase, its pending prompt and its attempt's cancellation (`Connections`), the private SSH directory (`SshStore`) | a second terminal model |
| Kotlin (`android/app`) | Activities, `SurfaceView` (`TerminalView`), the IME connection and its composing text, gesture classification, the key row, dialogs, the window selector, document picker, clipboard, the profile form's text (`SharedPreferences`) | pane cache, window list copy, wire protocol, connection state, prompt ownership, committed text |

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
compiled out.

## Connection

The mux has no domain until the user connects. `sshmux::connect` validates
`ProfileFields` into a `Profile` (tailnet address or `*.ts.net` name, port,
login name, optional absolute remote `wezterm` path), registers one
`ClientDomain` built from the existing `SshDomain` config and calls
`ClientDomain::attach_with_ui(None, ui)`: the desktop attach with no primary
window, so the laptop's windows map to mux windows and an empty laptop mux
creates nothing. No `LocalDomain`, server publisher, Lua startup hook or
spawn-if-empty exists on this path. The proxy command is derived as
`<quoted remote wezterm> cli --prefer-mux --no-auto-start proxy`; the user
cannot enter one. Each attempt registers its own domain (`laptop-<attempt>`)
and unregisters it when it closes (below).

SSH options come only from the profile: `wezterm_ssh::Config`
reads no configuration file on Android, `userknownhostsfile` and
`identityfile` point into `<filesDir>/ssh` (mode `0700`; files `0600`),
`identitiesonly=yes`, no agent, and `wezterm_ssh_destination_networks`
holds the tailnet ranges. `wezterm_ssh::DestinationNetworks` is the one
address validator: the profile checks a literal address with it, and the
SSH thread resolves a `*.ts.net` name once and dials the first resolved
address it accepts, so no other address is ever dialed and nothing is
resolved twice. Host trust stays keyed by the name the user entered.
Host verification, `known_hosts` updates, authentication and the codec
check are the native libssh and `wezterm-client` code, unchanged.

`mux::connui::ConnectionUI::with_consumer` hands the attach's UI requests to
the thread `wezterm-connect-ui`. Progress text, `Input` (echo off is a
secret) and the typed `HostTrust` request become state in
`connection::Connections`: phase `idle`, `attaching` or `attached`, or for
an ended attempt `cancelling`, `failing` or `disconnecting` while it is
closing and then `cancelled`, `failed` or `disconnected`; an attempt id and
at most one prompt with a one-use id.
The prompt's promise moves into that state before Kotlin is told
(`PlatformRequest::ConnectionChanged`). It leaves through one answer or
cancellation carrying both ids, the end of its attempt, or the end of the
engine; anything later is refused. One connect operation runs at a time.
Activities and surfaces are not part of this state: `ConnectionPanel`
renders `nativeConnectionStatus` and re-shows a pending prompt after a
restart. Connect is refused (`starting`) until the engine runs; the status
carries `ready` and the engine posts `ConnectionChanged` when it starts
running, which enables the button. Failures are classified by error type
(`mux::ssh::SshConnectError`, `IncompatibleVersionError`,
`VersionCheckFailed`), never by message text. A trusted key of another
type than the one presented (libssh `KnownHosts::Other`) reaches it as
`SessionEvent::HostKeyTypeChanged`, stage `HostKeyTypeChanged`, and fails as
`host_key_changed` without a prompt or a `known_hosts` write; the desktop
text stays the libssh error.

Each attempt owns one `wezterm_ssh::Cancel`, which `Connections::begin`
creates and keeps. It moves once, from active to committed or to
cancelled, and counts the attempt's workers: the caller of `begin` until
it hands off, the `wezterm-connect-ui` consumer, the connect thread of
`attach_with_ui`, the SSH session thread, the proxy's stderr reader and
child waiter, and the client thread. `ConnectionUI::with_consumer(cancel)`
carries it through the unchanged desktop call chain; every desktop
`ConnectionUI` has none, so desktop connections behave as before. The
attempt's owners are those workers and its mux domain. An ended attempt
is closing until every owner is gone, and ended only then:

- Cancel (`cancel_attempt`, from the calling thread, no GUI task): the
  prompt ends as cancelled, the transport shuts down, the phase is
  `cancelling` until the owners are gone and then `cancelled`. It is refused
  once `attach_with_ui` committed, which it does right before
  `finish_attach`, so nothing of a cancelled attempt is published and a
  late completion changes nothing.
- Shutdown wakes a TCP connect in progress (nonblocking connect polled with
  the cancel's wake socket), shuts the receiving side of the dialed socket
  (every libssh or libssh2 wait for the server reads end of file) and
  wakes the session's request loop, which then ends. The sending side
  stays open: libssh closes its socket when a write fails and then polls
  that closed socket without end. A trust answer after the shutdown writes
  no `known_hosts`. `getaddrinfo` cannot be interrupted; a cancel during a
  name lookup is seen when it returns, and nothing is dialed. The lookup
  is bounded only by the resolver's own timeouts.
- Disconnect (`disconnect`): `disconnecting`, then a GUI task detaches the
  domain, which removes its panes without a `KillPane` because a detached
  domain's `ClientPane::kill` sends none, then shuts the transport down;
  `disconnected` (cause `user`) when the owners are gone.
- A failed attach (`finish` with an error) shuts the transport down too, so
  a client the attach had already started ends; `failing`, then `failed`.
- The workers of an attached connection ending on their own make it
  `disconnecting` and then `disconnected` (cause `lost`), whether or not
  the laptop had panes.
- When the workers have ended and the domain is still registered,
  `Connections` calls its `retire` hook. One GUI task detaches the domain
  (removing its panes while it is still registered and detached, so no
  `KillPane`), unregisters exactly that domain id (`Mux::remove_domain`;
  the by-name entry and the default domain only if they are the same
  domain) and reports `retired`. An attached connection keeps its domain,
  also while the laptop has no panes.
- The engine's end shuts the transport down, and a registered domain
  becomes `stranded`: no GUI task can reach it any more, so it is reported
  as such, not counted as gone, and no longer holds the attempt. The
  `wezterm-connect-ui` consumer stops on the engine's end too, because an
  attach task stranded with the GUI thread keeps its `ConnectionUI`.
  Every request carries a `connui::Respond`, which answers
  `BrokenPromise` when it is dropped unanswered, so a request the
  consumer had not yet registered, or one still queued when it drops its
  receiver, resumes the thread that asked; a request sent after that
  fails at once.
- No new attempt starts while the previous one has an owner (`closing`).
  Reconnect is a new attempt: a new `laptop-<attempt>` domain, a new client
  and a new pane list. Nothing reconnects by itself, and input never
  waits for a connection.

The engine sets `quit_when_all_windows_are_closed=false` as a config
override: a disconnect or the laptop's last pane exiting empties the mux,
which must not end the engine. A process started again after Android stopped it is
`idle` until the user connects.

The identity is imported through `ACTION_OPEN_DOCUMENT`, read with a
bounded loop that works from API 24 (at most 1 MiB plus one byte; a
zero-byte read is an error), validated as a PEM or OpenSSH private key of
at most 1 MiB (OpenSSH `MAX_KEY_FILE_SIZE`, `authfile.c` through 8.x) and
written `0600` through a synced temporary file and a rename. It is a
file, not a Keystore key. The manifest disables backup and excludes every
domain from cloud backup and device transfer. Release builds log at info
level; SSH and attach errors at that level name the endpoint address.
Prompt answers and key bytes are never logged. Debug builds log at debug
level, which includes pane titles and working directories from the laptop,
but not typed, composed, pasted or shaped text: `TermWindow` logs window
events as `WindowEvent::without_text` on Android, the client logs a
PDU's name where upstream logs the PDU and a laptop's clipboard copy
(OSC 52) as its selection and byte count, and the logger caps
`wezterm_font::shaper` at info. The font and glyph records that would
hold screen text log metadata on Android, at their own level: a fallback
font that fails to load or size logs the byte count it could not shape
(not the error, which can quote it), missing glyphs log how many
codepoints, and a failed or debug glyph load names no glyph. Desktop
logs them unchanged.
`debug_key_events` (default false) is upstream's explicit opt-in and
still logs keys at info.

## Input

`TerminalView` turns every act into one typed JNI call
(`nativeInputPreedit`, `nativeInputCommit`, `nativeInputKey`,
`nativeInputPaste`, `nativeInputTouch`); it posts a
`PlatformEvent::Input` to the `EngineGate` and returns. The gate refuses
input unless the engine runs, and the GUI thread delivers each input to
the bound window or drops it (`input.dropped`), so input never waits for
an engine, a surface or a connection and is never replayed. Without a
shown laptop pane the view is not focusable and opens no IME connection.

`wezterm_android::input::window_events` (pure, host-tested) maps an input
to the window events the desktop backends send, and `TermWindow` handles
them unchanged:

- composing text is `AdviseDeadKeyStatus(Composing)`: drawn at the
  cursor, never sent;
- a commit is its `Erase` as Backspace key events, then one
  `KeyEvent(Char)` per character, then `AdviseDeadKeyStatus(None)`; `\n`
  becomes Enter. Not a
  composed write:
  the mux server writes key presses, pastes and mouse reports through its
  terminal's writer thread (`term` `ThreadedWriter`) but `WriteToPane`
  straight to the pty, so a write right after a key press (an IME
  deleting, then committing) can reach the program first;
- a key press maps the Android key code, its character and the meta
  state; Ctrl and Alt armed on the key row apply to the next key or single
  committed character;
- a paste carries the clipboard text Kotlin read on the UI thread (the
  same read that answers `clipboard_get`) and is `DroppedString`, whose
  handler `send_paste`s to the active pane at once, so it stays in order
  with the keys after it and the server applies bracketed paste. A key
  the key table binds to `PasteFrom` takes the same path: `TerminalView`
  asks `nativeIsPasteKey`, which looks the key up in the real `InputMap`
  on the calling thread (Android loads no config file, so these are the
  defaults: Ctrl+Shift+V, Super+V, Shift+Insert, the Paste key).
  `TermWindow`'s `PasteFrom` would read through the request queue and be
  overtaken by the keys after it;
- a tap is a left click, a long press then drag is a left-button drag
  (`TermWindow` selects and copies on release through `set_clipboard`),
  and a drag scrolls one wheel step per cell height crossed. The cell size
  is what the bound window reports to `set_text_cursor_position` while it
  paints.

Each IME connection is a `TerminalInputConnection`. It keeps `sent`,
what the laptop holds before its cursor because this connection typed it,
and its `Editable` is that text with the IME's edits applied, committed or
composing, so `getTextBeforeCursor`, `getExtractedText` and
`updateSelection` describe what the edits apply to. When the IME's
outermost batch ends, the laptop is brought to `remoteText` (pure,
JVM-tested): the `Editable`'s text when nothing composes; while something
composes, only committed text before the composition that extends `sent`.
Any other change, an erase above all, waits for the composition to end,
because a composition may stand for sent text the IME re-marked
(`setComposingRegion`), and the laptop keeps that text until the
recomposition is committed. The laptop erases the sent characters after
the first difference (one Backspace per grapheme cluster, as bash's
readline, zsh and vim erase a letter with its combining marks) and types
what follows; a difference inside a cluster erases it and retypes what
remains. Ranges that would split a surrogate pair are refused. Only the
newest connection edits. A key, a paste or a tap first commits and sends
the unsent text and forgets `sent`, as does a commit with a control
character or an armed modifier and a new input target; an edit of sent
text not on the laptop yet goes with it. Losing window or view focus drops
unsent text, so nothing composed before Back, Home or a dialog is typed
later.

The input target is the bound window's active pane (local id) and a
generation that counts its changes. Every write of a tab's active pane
is announced in the same call, with mux state locked: `PaneFocused(pane)`
by a focus change, `TabResized(tab)` by a resync
(`Tab::sync_with_pane_tree` sets the pane silently, then resizes), and
`WindowInvalidated`, `TabAddedToWindow` or `WindowRemoved` by a tab
change. The mux subscriber only records these as `TargetChange`s
(`wezterm_gui::android`), in order, into one queue; every GUI-thread
observation (after a window selection, before each input, and one
deferred per notification) drains it and moves the generation when the
active pane differs or any recorded change resolves to the bound window
(or no longer resolves), so a laptop focus A to B to A, or a resize of
the shown tab, also moves it. The target is published in the surface
status (`input_pane`, `input_generation`) with `WindowsChanged`;
`TerminalActivity` hands it to the view, which forgets `sent` on a new
one. A commit that erases carries the target it typed under, and the GUI
thread refuses the whole commit (`input.refused`) under any other: the
laptop may switch panes, and edit them, while the view has not yet learnt
it. Commits that only add text, and keys, go to the current pane.

Focus: on Android `SetFocusedPane` goes out only for a focus this client
chose (a tap on a pane, showing a window). The server announces every
focus change to every client, twice when it changed something;
`wezterm-client`'s `FocusAdvice` adopts an announced or resynced focus
without advising it back, and remembers the announced pane in place of
the advised one when it replaced that pane in the same window, so a later
paint has nothing to send. The adoption is taken under
`cfg!(target_os = "android")` at its two call sites; desktop clients keep
upstream's echo, so their server-side focus record (`list-clients`, the
CLI's default pane) still follows what they show. The GUI frontend's
deferred reconciliation of a `PaneFocused` notification (shared code)
runs only while that pane is still its tab's active pane. Without either,
two focus changes before the server answered (two taps, or the laptop
moving twice) alternate between the phone and the server without end.

Hardware keys: a character comes from the key's layout; right Alt alone
is AltGr when the layout gives the key a character with it, any other Alt
is sent as Alt; a dead key's accent waits (shown as composing text) and
combines with the next character through `KeyCharacterMap.getDeadChar`,
or is sent before it. Escape is taken in `onKeyPreIme`, before a shown
soft keyboard would use it to close itself. System keys (Back, volume)
and lone modifiers stay with the platform. `Input`'s `Debug` shows no
text, key or character.

The surface is what `systemWindowInsets` leaves free (with
`adjustResize` they include the soft keyboard; observed on API 24 and 35)
above the key row; `stateHidden` keeps the keyboard closed until a tap.
Every resize reaches the laptop pane through `TermWindow`'s resize. A
resync can apply a pane tree the laptop sent before the phone's last
resize (its `ListPanes` answer crossed the `Resize`), which resizes the
tab and the laptop's panes back; `TermWindow` would not notice, since
its surface did not change. So every `TabResized` makes
`fit_bound_window` post `TermWindowNotif::Apply`, and so does every
surface event after which the bound window shows, and every change of the
bound window, a selection or the bound window's close
(`window::os::android::on_rebind`), if it shows
(`Connection::presents`: bound, with a present surface), because a
window shown again at the size it had gets a `Resized` that
`TermWindow::resize` ignores. The closure decides when it runs: a
window that does not present (after Home, with the screen off) changes
nothing, so the laptop's resizes stand until the phone shows the window
again; one that presents calls `apply_dimensions` with its own
dimensions if a tab's rows or columns differ from
`current_cell_dimensions()`. Its own `TabResized` notifications are
ignored (a tab clamped to its split minimum never loops); a fitted tab
triggers nothing. While shown, a tab keeps the phone's size against any
other client; two phones showing it at different sizes resize it back
and forth without end.

`nativeSurfaceStatus` adds `input` (applied preedits, commits, keys,
pastes, touches, dropped, refused), the input target and the painted
cursor cell (`cursor_x`,
`cursor_y`, `cell_width`, `cell_height`). Debug builds add
`nativeDiagnosticActivePane`: the bound window's active pane with its
laptop id, size, cursor and viewport rows (plus up to two viewports of
scrollback).

## Stage status

ANDROID-01: the full GUI closure cross-builds, links and initializes;
`DiagnosticActivity` shows the report.

ANDROID-02: `TerminalActivity` binds one logical window to its
`SurfaceView` and `TermWindow` renders through wgpu into it. Debug
builds launched with the boolean extra
`org.wezterm.android.DIAGNOSTIC_APPLET` open a termwiz diagnostic applet
(`wezterm_gui::android::diagnostic`, no shell, no domain) whose literal
grid is the device evidence of the surface and lifecycle suites. Mux starts
with no default domain. `front_end`
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

ANDROID-03: the ordered shutdown, the platform event mailbox, logical
windows with a Kotlin selector, the request queue and the clipboard
described above. Debug builds drive them through `nativeDiagnosticGui`
(`open-window`, `paste`, `panic-on-queued-destroy`,
`panic-with-clipboard-read`, `panic-in-surface-lost`).

ANDROID-04: the connection described above. A normal
launch shows the connection screen. `nativeDiagnosticMux` (debug) lists
every mux domain and every pane with its local and laptop ids. Verified
only against the owned fixtures of `ci/android-sshmux-fixture.sh`, reached
through the host's own Tailscale address over `lo`: no private Tailscale
flow, no laptop other than the build host and no phone has been exercised.

ANDROID-05: the input described above. Debug builds hold and release
the deferred input-target observations (`nativeDiagnosticGui`
`hold-target-observations`, `release-target-observations`,
`nativeDiagnosticHeldObservations`) and replay a kept pane tree through
`Tab::sync_with_pane_tree` (`snapshot-bound-tab`,
`apply-bound-tab-snapshot`), and hold the bound window's fits (`hold-fits`;
`release-fits` runs one on the GUI thread and returns once its closure is
queued, false if none was held), and hold platform input (`hold-input`;
`release-input` applies it in one GUI-thread task, in arrival order;
surface events are not held, so held input must not itself change the
surface, as a tap that opens the soft keyboard does).
Verified only on emulators against
the owned fixtures over `lo`, with input generated by instrumentation
(including touches on the installed soft keyboard's keys); no phone,
hardware keyboard or human typing has been exercised.

ANDROID-06: the cancellation, disconnect and reconnect described above.
`ConnectionPanel` shows Cancel while attaching, "Stop connecting" in its
prompt dialogs, Disconnect in the empty state and Reconnect after a
disconnect; the key row's "…" key asks before disconnecting. Debug builds
add `nativeDiagnosticConnection`: `interrupt-transport` shuts the latest
attempt's transport down as a failing network would, `census` lists the
process's threads and descriptors and the attempt's workers. Verified only
on emulators against the owned fixtures over `lo`.

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
  `build` runs Gradle lint, which fails on a framework call above minSdk
  (`NewApi`).
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
- `test <serial> <suite>` requires an explicit serial and runs `check`
  for its ABI first. Gradle only assembles `assembleDebug` and
  `assembleDebugAndroidTest`; the runner installs both APKs and invokes
  `am instrument` through `adb -s <serial>`. No device enumeration or
  Gradle connected/provider task is used. `ci/android_instrument.py`
  owns the exact method inventory and process plan. Run its negative
  controls with `uv run --no-project python ci/test_android_instrument.py`.
  Parsing requires the actual adb exit, paired literal start/completion
  records for every expected method and the final runner result; failures,
  errors, skips and incomplete runs fail the command. Suites:
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
  on a surface destroy and inside the shutdown), `sshmux`
  (`SshMuxStartTest` and `SshMuxTest`, one method per process: Connect
  before the engine runs, the bounded key read, the connection screen,
  addresses outside the tailnet, host trust rejected, accepted, persisted,
  changed and changed to another key type, a prompt across Activity
  restarts, key import through the system picker including documents of
  exactly 1 MiB and one byte more, attach with pane ids compared to the
  laptop's, passphrase and password prompts, a missing server, a codec
  mismatch, an empty server; then `FontLogTest`, one method per process
  on the `lastpane` shell with a test fallback font: a fallback font file
  gone before its first load, its wide character drawn as a placeholder
  that keeps both cells (read from a screenshot), and a codepoint no font
  covers, each logged without the text) and `input` (`InputTest`, one process, its
  methods in name order on one connection: a command typed through the
  `InputConnection`, composition, deletion around surrogates and combining
  marks, hardware keys and the key row, paste (the IME's, the key row's
  and the hardware paste keys, each followed by Enter in the same UI
  turn), touch selection and scrolling, keyboard insets and rotation
  against `stty size`, vim with mouse clicks, switching windows
  mid-composition, Back with a pending composition, per-commit latency with
  the idle-redraw check, recomposition of sent text, a rewrite refused
  after the laptop moved its focus to another pane, and after it moved
  away and back with the observations held, and a stale pane tree after
  rotation fitted back to the surface, and a laptop resize that stands
  while the phone shows nothing and is fitted when it shows again, with
  the fits held around Home, two pane taps in one GUI-thread task (input
  held) sent once and not echoed, and the window shown after the laptop
  closed the shown one fitted to the surface, and a laptop OSC 52 copy
  reaching the phone's clipboard; first, the
  installed soft keyboard typing a command through real touches on its keys,
  found in its own accessibility tree) and `reconnect` (`ReconnectTest`,
  one method per process: cancel during the TCP connect (twice, then no
  new thread or socket inode), host trust, authentication, the version
  check and the pane list; a lost connection, input queued across it and
  sent while disconnected dropped, an explicit reconnect to the same laptop
  pane ids through a fresh mapping, and a user disconnect; an attach the
  end of its instrumentation force-stops, then a relaunch that reattaches
  the same panes; the deliberate exit of the laptop's last pane; a network
  loss with a host-trust answer after it and Home, then a loss while Back
  hid the surface; three rounds of a cancelled connect and a user
  disconnect, after each of which no worker, prompt or mux domain remains,
  and after the third no thread or socket beyond the first round's, each
  round's thread and descriptor maps logged whole before and after it; a GUI
  engine failure while an attempt waits for the version answer, after which
  its threads, the connection UI thread among them, end and its domain is
  reported stranded; the same failure while a debug hold keeps the
  password request unregistered and unanswered, after which the session
  thread that asked still resumes and every thread ends).
  `sshmux`, `input` and `reconnect` need
  `ci/android-sshmux-fixture.sh up <address>` first: an
  owned sshd bound to loopback or the host's own Tailscale address, owned
  mux servers with `HOME` and `XDG_RUNTIME_DIR` inside
  `target/android-sshmux-fixture`, and generated keys. Its `input` server
  has a bash pane in window 0, in window 1 a capture pane that prints the
  hex of every byte it receives (bracketed paste on, six bytes per line)
  and appends it to `capture.hex`, and in window 2 two such captures
  stacked (`capture-a.hex`, `capture-b.hex`): the top one runs
  `wezterm cli activate-pane` for the bottom one once it has read `abc`,
  the bottom one activates the top one and then itself once it has read
  `aba`. The shell's `PATH` has `laptop-resize <rows> <cols>`: the
  `sshmux_resize_pane` example, a laptop client that sends the server
  the `Resize` a laptop GUI sends for the shell's own pane, then
  `stty size` on a cleared row, and `laptop-cli`, the laptop's own
  `wezterm cli` on that server. `input` types into those
  panes, so run it against a fresh `up`. For `reconnect` the fixture adds a
  listener whose full accept queue holds every TCP connect in progress
  (`sshmux_stall listen`), proxies that never answer the version or the
  pane list (`sshmux_stall version|list`, logging what they wait on), a
  `reconnect` server whose one window is a capture pane
  (`capture-reconnect.hex`), and a `lastpane` server whose one bash pane
  the phone closes by typing `lastpane-cli kill-pane`, the laptop's own
  CLI; `reconnect` types into them, so it too needs a fresh `up`. `down` also stops the server a desktop client without
  `--no-auto-start` starts for the missing-server account, found through
  its own pid file in the fixture's runtime directory. The runner copies
  the fixture endpoint and keys to the device for the suite and removes
  them in the exit trap; if the fixture's empty server did not survive,
  `BLOCKED-empty.txt` records the blocker and the command fails without
  omitting a required method. Fixture results are never private-route
  evidence. The entries of a suite
  (class or `class#method`) run one process each because the engine starts
  once per process; literal JUnit is written to
  `android/app/build/outputs/androidTest-results/<suite>/<entry>.xml`.
  Sibling `.command`, `.instrument`, `.exit` and `.logcat*` files preserve
  execution and capture receipts in a private directory. Prior result
  directories move to `<suite>.previous.*/results`, not deletion. Each
  logcat child is launched through `exec`, then terminated and waited for
  before parsing or exit. The runner never kills the shared adb server.
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
