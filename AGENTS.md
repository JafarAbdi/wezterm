# Core and Android application boundary

This repository retains the desktop workspace and reusable Android OS support.
The Android application is a separate repository; no Kotlin/Gradle, JNI/logger,
app workspace membership, build/test runner or Android CI belongs here.

Core owns the Android `window` backend/message loop, logical-window surface
binding, native-window/GPU leases and clipboard request ownership; GUI library,
desktop entry, native renderer/fonts, private frontend and `TermWindow` adapters;
shared mux/client/SSH cancellation, trust/network/focus behavior and tests.
`config::AndroidPaths` supplies paths without mutating the JVM environment.
Native font APIs receive typed absolute point sizes, not phone dp/default/minimum
policy. App-supplied defaults must survive ordinary config reload and
`overridden_config`; explicit desktop config/default styles remain unchanged.

The app owns startup/mux-client registration, image-store initialization and
retention across fatal parking, diagnostic applet, lifecycle/JNI/input/profile/
connection state, appearance policy/persistence and UX. App messages travel as
opaque payloads through the existing window request queue; clipboard promises
remain in core. Narrow assignment/input-target/fitting adapters preserve native
scheduler and terminal-window ownership without exposing private modules.

Desktop session persistence remains part of the shared mux. Android application
builds consume this workspace as a dependency; their packaging and release
configuration belong to the separate application repository.
