// Don't create a new standard console window when launched from the windows GUI.
#![cfg_attr(not(test), windows_subsystem = "windows")]

fn main() {
    wezterm_gui::desktop::main()
}
