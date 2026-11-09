//! SoundSight 7.1 Setup: installs or removes the "SoundSight 7.1" virtual speaker device.
//!
//! Games play 7.1 into that device, SoundSight captures it with WASAPI loopback
//! exactly like any other output device. The driver itself lives in `driver/`.
//!
//! Usage: `SoundSight71Setup.exe [install|uninstall] [--quiet]`.
//! With no arguments it asks what to do. Exit code 0 = success.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod payload {
    include!(concat!(env!("OUT_DIR"), "/payload.rs"));
}

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("SoundSight71Setup only runs on Windows.");
    std::process::exit(1);
}
