//! SoundSight-RS: visual sound radar from WASAPI loopback audio.
//!
//! `dsp`, `config` and `state` are platform independent (and unit tested on any OS);
//! the capture/overlay/tray code is Windows-only.

// Per-channel loops index several parallel fixed-size arrays at once; iterator
// zips would be less readable and compile to the same code.
#![allow(clippy::needless_range_loop)]
pub mod config;
pub mod dsp;
pub mod hotkey;
pub mod render;
pub mod state;

#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod ui;
