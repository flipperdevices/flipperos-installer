//! FlipperOS installer library: the installer engine plus optional frontends.

pub mod core;

#[cfg(feature = "gui")]
pub mod gui;

#[cfg(feature = "tui")]
pub mod tui;
