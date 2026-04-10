//! Pure Rust RIST (Reliable Internet Stream Transport) protocol implementation.
//!
//! Implements VSF TR-06-1:2020 (Simple Profile) and TR-06-2:2024 (Main Profile).
//! This crate contains no I/O — only packet parsing, serialization, and protocol
//! state machines. See `rist-transport` for the async networking layer.

pub mod config;
pub mod error;
pub mod packet;
pub mod protocol;
pub mod stats;

// Stubbed modules for future phases
pub mod crypto;
pub mod gre;
