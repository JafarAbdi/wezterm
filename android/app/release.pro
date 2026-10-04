# Instrumentation shares the app's Kotlin runtime instead of packaging its own.
-keep class kotlin.** { *; }

-keep class org.wezterm.android.NativeApp {
    public org.wezterm.android.InitResponse initialize(android.content.Context);
}
