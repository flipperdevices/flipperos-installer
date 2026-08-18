//! Core installer engine and shared state, independent of any frontend.

pub mod archive;
pub mod board;
pub mod bundle;
pub mod catalog;
pub mod controller;
pub mod fetch;
pub mod install;
pub mod layout;
pub mod menu;
pub mod model;
pub mod power;
pub mod provision;
pub mod removable;
pub mod stage;
pub mod storage;
pub mod ufs;

pub use controller::{Config, Controller};
