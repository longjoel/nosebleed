//! Public library surface for the `nosebleed` runtime.
//!
//! The binary target provides the turnkey CLI/server. The library target exposes
//! protocol types, frame/audio/input primitives, authentication helpers, and the
//! Axum server/session building blocks for hosts that want to embed or wrap the
//! runtime instead of spawning the CLI.

use std::sync::PoisonError;

/// Recover from a poisoned lock by logging the event and proceeding with the
/// inner value. This should never silently surrender — every poison event is
/// a sign of a panic on the lock-holding thread and deserves visibility.
pub fn lock_recover<G>(err: PoisonError<G>) -> G {
    eprintln!(
        "[nosebleed] recovered from poisoned lock — a prior panic occurred on the lock-holding thread"
    );
    err.into_inner()
}

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

pub mod gstreamer_backend;

pub mod hw_render;
