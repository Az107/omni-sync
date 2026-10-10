//! The omni-sync server library.
//!
//! `main.rs` is a thin CLI around this: build a [`state::AppState`], hand its
//! [`app::router`] to axum, and serve. Keeping the app in a library lets tests
//! drive the router in-process.

pub mod app;
pub mod auth;
pub mod config;
pub mod state;
pub mod templates;
