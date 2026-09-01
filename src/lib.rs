mod capture;
pub mod capture_test;
pub mod client;
pub mod config;
mod connect;
mod crypto;
mod dns;
mod emulation;
pub mod emulation_test;
mod listen;
pub mod service;
mod sync;

/// Signal that this process runs a serviced main dispatch queue (a GUI
/// host such as the Tauri app). Required before macOS keyboard-layout
/// sync will do anything, because the macOS Text Input Source APIs must
/// be called on the main thread. Call once at startup from the GUI host.
pub fn set_gui_host() {
    sync::set_gui_host();
}
