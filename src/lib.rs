//! SoundSight-RS: visual sound radar from WASAPI loopback audio.
//!
//! `dsp`, `config` and `state` are platform independent (and unit tested on any OS);
//! the capture/overlay/tray code is Windows-only.
pub mod config;
pub mod dsp;
pub mod state;

#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod ui;
