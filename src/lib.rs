//! FlipperOS installer library: the installer engine plus optional frontends.

pub mod core;
pub mod version;

#[cfg(feature = "gui")]
pub mod gui;

#[cfg(feature = "tui")]
pub mod tui;

#[cfg(feature = "web")]
pub mod net;

#[cfg(feature = "web")]
pub mod web;
