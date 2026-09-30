#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(all(feature = "wayland", not(target_os = "android")))]
pub mod wayland;
#[cfg(not(target_os = "android"))]
pub mod x11;
#[cfg(not(target_os = "android"))]
pub mod x_and_wayland;
#[cfg(not(target_os = "android"))]
pub mod xdg_desktop_portal;
#[cfg(not(target_os = "android"))]
pub mod xkeysyms;

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
pub use x_and_wayland::*;

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub use self::android::*;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::*;

pub mod parameters;
