use std::io::{Read, Write};
use std::net::TcpListener;

use anyhow::{Context, Result};

use crate::x11::proto::{build_setup_success, parse_setup_request, ByteOrder};

/// Run a minimal X11 server that accepts a single connection, performs the setup handshake,
/// then closes. This is the first end-to-end slice to validate parsing/serialization.
pub fn run_single_handshake(addr: &str) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
    let (mut stream, peer) = listener.accept().context("accept client")?;
    println!("x11 client connected from {peer:?}");

    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).context("read setup request")?;
    let (req, used) = parse_setup_request(&buf[..n]).context("parse setup request")?;
    if n > used {
        // Keep it simple: ignore extra bytes; later we can buffer for first request.
    }

    let byte_order = req.byte_order;
    let reply = build_setup_success(byte_order);
    stream.write_all(&reply).context("write setup reply")?;
    stream.flush().ok();

    Ok(())
}

// Placeholder for future conversions when we parse requests with x11rb types.
