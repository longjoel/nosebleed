use std::io::{Read, Write};
use std::net::TcpListener;

use anyhow::{Context, Result};

use crate::x11::proto::{build_setup_success, parse_setup_request, ByteOrder};

#[derive(Debug)]
struct Framebuffer {
    width: u16,
    height: u16,
    data: Vec<u8>, // ARGB8888
}

impl Framebuffer {
    fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize * 4;
        Self {
            width,
            height,
            data: vec![0; len],
        }
    }

    fn put_image(&mut self, x: i16, y: i16, w: u16, h: u16, data: &[u8]) {
        // ZPixmap only; ARGB8888 assumed.
        let bytes_per_pixel = 4usize;
        for row in 0..h {
            let dst_y = y as isize + row as isize;
            if dst_y < 0 || dst_y >= self.height as isize {
                continue;
            }
            let src_off = row as usize * w as usize * bytes_per_pixel;
            let dst_x = x.max(0) as usize;
            let max_x = (x as isize + w as isize).min(self.width as isize);
            if max_x <= dst_x as isize {
                continue;
            }
            let copy_w = (max_x as usize - dst_x) * bytes_per_pixel;
            let dst_index =
                (dst_y as usize * self.width as usize + dst_x) * bytes_per_pixel;
            let src_index = if x < 0 {
                src_off + (-x as usize) * bytes_per_pixel
            } else {
                src_off
            };
            let end = src_index + copy_w;
            if end <= data.len() && dst_index + copy_w <= self.data.len() {
                self.data[dst_index..dst_index + copy_w]
                    .copy_from_slice(&data[src_index..end]);
            }
        }
    }
}

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

    // Minimal request loop: consume requests and handle PutImage and CopyArea on root framebuffer.
    let mut fb = Framebuffer::new(1280, 720);
    loop {
        let mut header = [0u8; 4];
        if let Err(err) = stream.read_exact(&mut header) {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(err).context("read request header");
        }
        let opcode = header[0];
        let minor = header[1];
        let req_len_words = byte_order.read_u16(&header[2..])?;
        if req_len_words == 0 {
            break;
        }
        let total_len = req_len_words as usize * 4;
        let mut body = vec![0u8; total_len.saturating_sub(4)];
        stream.read_exact(&mut body)?;

        match opcode {
            72 => handle_put_image(&mut fb, minor, &body, byte_order).context("PutImage")?,
            62 => handle_copy_area(&mut fb, &body, byte_order).context("CopyArea")?,
            1 => { /* CreateWindow - ignore for now */ }
            8 => { /* MapWindow - ignore for now */ }
            _ => {
                // For now, ignore unsupported opcodes.
            }
        }
    }

    Ok(())
}

fn handle_put_image(
    fb: &mut Framebuffer,
    format_byte: u8,
    body: &[u8],
    order: ByteOrder,
) -> Result<()> {
    if body.len() < 20 {
        return Ok(());
    }
    if format_byte != 2 {
        // Only support ZPixmap.
        return Ok(());
    }
    let drawable = order.read_u32(&body[0..4])?;
    let _gc = order.read_u32(&body[4..8])?;
    let width = order.read_u16(&body[8..10])?;
    let height = order.read_u16(&body[10..12])?;
    let dst_x = order.read_i16(&body[12..14])?;
    let dst_y = order.read_i16(&body[14..16])?;
    let _left_pad = body[16];
    let _depth = body[17];

    let data = if body.len() > 20 { &body[20..] } else { &[] };

    if drawable == 0x2000_0000 {
        fb.put_image(dst_x, dst_y, width, height, data);
    }
    Ok(())
}

fn handle_copy_area(fb: &mut Framebuffer, body: &[u8], order: ByteOrder) -> Result<()> {
    if body.len() < 28 {
        return Ok(());
    }
    let src_drawable = order.read_u32(&body[0..4])?;
    let dst_drawable = order.read_u32(&body[4..8])?;
    let _gc = order.read_u32(&body[8..12])?;
    let src_x = order.read_i16(&body[12..14])?;
    let src_y = order.read_i16(&body[14..16])?;
    let dst_x = order.read_i16(&body[16..18])?;
    let dst_y = order.read_i16(&body[18..20])?;
    let width = order.read_u16(&body[20..22])?;
    let height = order.read_u16(&body[22..24])?;

    if src_drawable == 0x2000_0000 && dst_drawable == 0x2000_0000 {
        copy_rect(fb, src_x, src_y, dst_x, dst_y, width, height);
    }
    Ok(())
}

fn copy_rect(fb: &mut Framebuffer, src_x: i16, src_y: i16, dst_x: i16, dst_y: i16, w: u16, h: u16) {
    let bpp = 4usize;
    let width = fb.width as usize;
    for row in 0..h {
        let sy = src_y as isize + row as isize;
        let dy = dst_y as isize + row as isize;
        if sy < 0 || dy < 0 || sy >= fb.height as isize || dy >= fb.height as isize {
            continue;
        }
        let sx = src_x.max(0) as usize;
        let dx = dst_x.max(0) as usize;
        let max_w = (w as isize).min(fb.width as isize - dx as isize);
        if max_w <= 0 {
            continue;
        }
        let count = max_w as usize * bpp;
        let src_index = (sy as usize * width + sx) * bpp;
        let dst_index = (dy as usize * width + dx) * bpp;
        if src_index + count <= fb.data.len() && dst_index + count <= fb.data.len() {
            // Use copy_within-safe temp to handle overlaps.
            let temp = fb.data[src_index..src_index + count].to_vec();
            fb.data[dst_index..dst_index + count].copy_from_slice(&temp);
        }
    }
}
