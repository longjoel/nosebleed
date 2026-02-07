use std::io::ErrorKind;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tungstenite::protocol::Message;

use crate::x11::server::Framebuffer;

pub fn serve_ws(
    host: &str,
    port: u16,
    fb: Arc<Mutex<Framebuffer>>,
    target_fps: u32,
) -> Result<()> {
    let listener = TcpListener::bind((host, port)).with_context(|| format!("bind ws {host}:{port}"))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let fb = Arc::clone(&fb);
                thread::spawn(move || {
                    if let Err(err) = handle_conn(stream, fb, target_fps) {
                        eprintln!("ws conn error: {err:#}");
                    }
                });
            }
            Err(err) => eprintln!("ws accept error: {err:#}"),
        }
    }
    Ok(())
}

fn handle_conn(stream: std::net::TcpStream, fb: Arc<Mutex<Framebuffer>>, target_fps: u32) -> Result<()> {
    let mut ws = tungstenite::accept(stream).context("handshake")?;
    ws.get_mut().set_nonblocking(true).ok();
    let frame_time = Duration::from_millis((1000 / target_fps.max(1)) as u64);
    loop {
        let start = Instant::now();
        match ws.read_message() {
            Ok(msg) if msg.is_close() => return Ok(()),
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
            Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(tungstenite::Error::ConnectionClosed) => return Ok(()),
            Err(_) => {} // ignore other transient errors
            _ => {}
        }
        let packet = fb
            .lock()
            .ok()
            .map(|g: std::sync::MutexGuard<'_, crate::x11::server::Framebuffer>| g.snapshot_packet());
        if let Some(pkt) = packet {
            ws.write_message(Message::Binary(pkt)).context("write frame")?;
        }
        let elapsed = start.elapsed();
        if elapsed < frame_time {
            thread::sleep(frame_time - elapsed);
        }
    }
}
