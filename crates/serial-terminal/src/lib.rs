//! The human terminal for SerialBridge.
//!
//! The TUI is split into seams that can be tested without a real terminal:
//! [`screen`] parses device bytes, [`state`] holds devices/sessions/notices,
//! [`input`] maps keys, [`render`] draws into a ratatui buffer, [`terminal`]
//! restores the terminal, and [`app`] makes the core calls.

pub mod app;
pub mod input;
pub mod render;
pub mod screen;
pub mod state;
pub mod terminal;
