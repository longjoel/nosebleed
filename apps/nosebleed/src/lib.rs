//! Public library surface for the `nosebleed` runtime.
//!
//! The binary target provides the turnkey CLI/server. The library target exposes
//! protocol types, frame/audio/input primitives, authentication helpers, and the
//! Axum server/session building blocks for hosts that want to embed or wrap the
//! runtime instead of spawning the CLI.

pub mod arcade;
pub mod audio;
pub mod auth;
pub mod core;
pub mod frame;
pub mod input;
pub mod libretro;
pub mod media;
pub mod protocol;
pub mod server;
pub mod session;

#[cfg(feature = "media-gstreamer")]
pub mod gstreamer_backend;
