#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    soundsight::ui::app::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("SoundSight-RS only runs on Windows (WASAPI + Win32 overlay).");
    std::process::exit(1);
}
