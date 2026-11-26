//! NitroRust library root so `cargo test --lib` runs all unit tests without UAC elevation
//! while `nitrorust.exe` keeps its `requireAdministrator` manifest.

pub mod acer;
pub mod app;
pub mod caps;
pub mod curve;
pub mod hw;
pub mod power;
pub mod proto;
pub mod settings;
pub mod win32;

slint::include_modules!();
