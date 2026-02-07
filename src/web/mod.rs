use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

/// Very small HTTP server that serves static/index.html and a /frame endpoint backed by a PNG supplier.
pub fn serve_http(
    host: &str,
    port: u16,
    root: &Path,
    frame_supplier: Arc<dyn Fn() -> Option<Vec<u8>> + Send + Sync>,
) -> Result<()> {
    let listener = TcpListener::bind((host, port)).with_context(|| format!("bind http {host}:{port}"))?;
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let root = root.to_path_buf();
                let supplier = Arc::clone(&frame_supplier);
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(&mut stream, &root, &supplier) {
                        eprintln!("http error: {e:#}");
                    }
                });
            }
            Err(err) => eprintln!("http accept error: {err:#}"),
        }
    }
    Ok(())
}

fn handle_conn(
    stream: &mut TcpStream,
    root: &Path,
    frame_supplier: &Arc<dyn Fn() -> Option<Vec<u8>> + Send + Sync>,
) -> Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).context("read request")?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = parse_path(&req);

    if path == "/frame" {
        if let Some(png) = (frame_supplier)() {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n",
                png.len()
            );
            stream.write_all(resp.as_bytes())?;
            stream.write_all(&png)?;
        } else {
            let resp = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
            stream.write_all(resp.as_bytes())?;
        }
        return Ok(());
    }

    // Default to index.html
    let path = if path == "/" { "/index.html" } else { path.as_str() };
    let file_path = root.join(path.trim_start_matches('/'));
    if file_path.exists() {
        let data = std::fs::read(&file_path).context("read file")?;
        let content_type = match file_path.extension().and_then(|e| e.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "text/javascript; charset=utf-8",
            _ => "application/octet-stream",
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            data.len()
        );
        stream.write_all(resp.as_bytes())?;
        stream.write_all(&data)?;
    } else {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        stream.write_all(resp.as_bytes())?;
    }
    Ok(())
}

fn parse_path(req: &str) -> String {
    if let Some(line) = req.lines().next() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            return parts[1].to_string();
        }
    }
    "/".to_string()
}
