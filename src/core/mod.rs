//! Core installer engine and shared state, independent of any frontend.

pub mod board;
pub mod catalog;
pub mod controller;
pub mod install;
pub mod layout;
pub mod model;
pub mod removable;
pub mod storage;

pub use controller::{Config, Controller};
