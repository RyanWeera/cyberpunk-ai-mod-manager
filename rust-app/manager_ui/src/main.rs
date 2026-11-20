//! The exe: a window app in release builds (no console), a console in debug builds for test output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    manager_ui::app::main();
}
