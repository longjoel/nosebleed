use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

const WIDTH: u16 = 320;
const HEIGHT: u16 = 240;
const ROOT_DRAWABLE: u32 = 0x2000_0000; // matches build_setup_success()

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to the nosebleed X11 server (set DISPLAY=127.0.0.1:6000 for consistency).
    let mut stream = TcpStream::connect("127.0.0.1:6000")?;
    stream.set_nodelay(true)?;

    send_setup(&mut stream)?;
    read_setup_reply(&mut stream)?;

    let mut x: i32 = 50;
    let mut y: i32 = 50;
    let mut vx: i32 = 2;
    let mut vy: i32 = 2;
    let radius: i32 = 30;

    loop {
        x += vx;
        y += vy;
        if x - radius < 0 {
            x = radius;
            vx = -vx;
        }
        if y - radius < 0 {
            y = radius;
            vy = -vy;
        }
        if x + radius >= WIDTH as i32 {
            x = WIDTH as i32 - radius - 1;
            vx = -vx;
        }
        if y + radius >= HEIGHT as i32 {
            y = HEIGHT as i32 - radius - 1;
            vy = -vy;
        }

        let mut buf = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
        for chunk in buf.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[0x10, 0x10, 0x18, 0xff]);
        }
        draw_ball(&mut buf, x, y, radius, WIDTH as usize, HEIGHT as usize);

        send_put_image(&mut stream, 0, 0, WIDTH, HEIGHT, &buf)?;
        thread::sleep(Duration::from_millis(16));
    }
}

fn send_setup(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut req = Vec::with_capacity(12);
    req.push(b'l');
    req.push(0);
    req.extend_from_slice(&11u16.to_le_bytes()); // protocol major
    req.extend_from_slice(&0u16.to_le_bytes()); // protocol minor
    req.extend_from_slice(&0u16.to_le_bytes()); // auth name len
    req.extend_from_slice(&0u16.to_le_bytes()); // auth data len
    req.extend_from_slice(&[0, 0]); // pad
    stream.write_all(&req)
}

fn read_setup_reply(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    let length_words = u16::from_le_bytes([header[6], header[7]]);
    let mut rest = vec![0u8; length_words as usize * 4];
    if !rest.is_empty() {
        stream.read_exact(&mut rest)?;
    }
    Ok(())
}

fn send_put_image(
    stream: &mut TcpStream,
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
    data: &[u8],
) -> std::io::Result<()> {
    let total_bytes = 4 + 20 + data.len();
    let length_words = ((total_bytes + 3) / 4) as u16;

    let mut req = Vec::with_capacity(total_bytes);
    req.push(72); // opcode PutImage
    req.push(2); // format ZPixmap
    req.extend_from_slice(&length_words.to_le_bytes());

    req.extend_from_slice(&ROOT_DRAWABLE.to_le_bytes());
    req.extend_from_slice(&0u32.to_le_bytes()); // gc
    req.extend_from_slice(&width.to_le_bytes());
    req.extend_from_slice(&height.to_le_bytes());
    req.extend_from_slice(&dst_x.to_le_bytes());
    req.extend_from_slice(&dst_y.to_le_bytes());
    req.push(0); // left pad
    req.push(24); // depth
    req.extend_from_slice(&[0, 0]); // pad

    req.extend_from_slice(data);
    stream.write_all(&req)
}

fn draw_ball(buf: &mut [u8], cx: i32, cy: i32, r: i32, width: usize, height: usize) {
    let r2 = (r * r) as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r2 {
                let px = cx + dx;
                let py = cy + dy;
                if px < 0 || py < 0 || px as usize >= width || py as usize >= height {
                    continue;
                }
                let idx = (py as usize * width + px as usize) * 4;
                buf[idx..idx + 4].copy_from_slice(&[0x3a, 0xc6, 0x5c, 0xff]);
            }
        }
    }
}
