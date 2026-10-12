//! `wordcraft-android`: WordCraft on Android tablets, published as Folio Writer.
//!
//! A native host without eframe or winit: `android-activity` (GameActivity) for the window, the
//! lifecycle and input, egui-wgpu for drawing, and the same `WordApp` as the desktop. Owning the event
//! loop is what gives mice, touchpads and keyboard modifiers (and, next, IME text), which winit's Android
//! backend drops. Only `android/` touches Android; the rest builds and is tested on any host.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// Off Android only the tests use the host.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

mod crash;
mod files;
mod host;
mod input;
mod keymap;
mod platform;
mod prompts;
mod recovery;
mod store;

#[cfg(target_os = "android")]
mod android;
