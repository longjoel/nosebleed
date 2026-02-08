//! Minimal X11 protocol types for connection setup (handshake) responses.
use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    LsbFirst,
    MsbFirst,
}

impl ByteOrder {
    pub fn read_u16(self, bytes: &[u8]) -> Result<u16> {
        if bytes.len() < 2 {
            bail!("not enough bytes for u16");
        }
        Ok(match self {
            ByteOrder::LsbFirst => u16::from_le_bytes([bytes[0], bytes[1]]),
            ByteOrder::MsbFirst => u16::from_be_bytes([bytes[0], bytes[1]]),
        })
    }

    pub fn read_u32(self, bytes: &[u8]) -> Result<u32> {
        if bytes.len() < 4 {
            bail!("not enough bytes for u32");
        }
        Ok(match self {
            ByteOrder::LsbFirst => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            ByteOrder::MsbFirst => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        })
    }

    pub fn read_i16(self, bytes: &[u8]) -> Result<i16> {
        self.read_u16(bytes).map(|v| v as i16)
    }

    fn write_u16(self, val: u16, out: &mut Vec<u8>) {
        let b = match self {
            ByteOrder::LsbFirst => val.to_le_bytes(),
            ByteOrder::MsbFirst => val.to_be_bytes(),
        };
        out.extend_from_slice(&b);
    }

    fn write_u32(self, val: u32, out: &mut Vec<u8>) {
        let b = match self {
            ByteOrder::LsbFirst => val.to_le_bytes(),
            ByteOrder::MsbFirst => val.to_be_bytes(),
        };
        out.extend_from_slice(&b);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupRequest {
    pub byte_order: ByteOrder,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub auth_name: Vec<u8>,
    pub auth_data: Vec<u8>,
}

pub fn parse_setup_request(buf: &[u8]) -> Result<(SetupRequest, usize)> {
    if buf.len() < 12 {
        bail!("setup request too short");
    }
    let byte_order = match buf[0] {
        b'l' => ByteOrder::LsbFirst,
        b'B' => ByteOrder::MsbFirst,
        other => bail!("invalid byte order byte {other}"),
    };

    let protocol_major = byte_order.read_u16(&buf[2..])?;
    let protocol_minor = byte_order.read_u16(&buf[4..])?;
    let auth_name_len = byte_order.read_u16(&buf[6..])? as usize;
    let auth_data_len = byte_order.read_u16(&buf[8..])? as usize;

    let mut offset = 12;
    let auth_name = read_padded(&buf, &mut offset, auth_name_len)
        .context("reading auth name")?
        .to_vec();
    let auth_data = read_padded(&buf, &mut offset, auth_data_len)
        .context("reading auth data")?
        .to_vec();

    Ok((
        SetupRequest {
            byte_order,
            protocol_major,
            protocol_minor,
            auth_name,
            auth_data,
        },
        offset,
    ))
}

fn read_padded<'a>(buf: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = *offset + len;
    if end > buf.len() {
        bail!("padded read out of range");
    }
    let slice = &buf[*offset..end];
    let pad = (4 - (len % 4)) % 4;
    let padded_end = end + pad;
    if padded_end > buf.len() {
        bail!("padding read out of range");
    }
    *offset = padded_end;
    Ok(slice)
}

/// Build a minimal success setup reply with one screen and one TrueColor visual.
pub fn build_setup_success(byte_order: ByteOrder) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);

    // Header placeholders; length is filled later.
    out.push(1); // success
    out.push(0); // unused
    byte_order.write_u16(11, &mut out); // protocol major
    byte_order.write_u16(0, &mut out); // protocol minor
    byte_order.write_u16(0, &mut out); // length placeholder

    // Core fields.
    byte_order.write_u32(0x0004_0000, &mut out); // release number
    byte_order.write_u32(0x1000_0000, &mut out); // resource_id_base
    byte_order.write_u32(0x0fff_f000, &mut out); // resource_id_mask (leaves upper bits usable)
    byte_order.write_u32(0, &mut out); // motion buffer size

    let vendor = b"nosebleed";
    byte_order.write_u16(vendor.len() as u16, &mut out); // vendor length
    byte_order.write_u16(0xffff, &mut out); // maximum_request_length

    let formats_count: u8 = 3;
    out.push(1); // screens
    out.push(formats_count); // pixmap formats
    out.push(0); // image byte order: LSBFirst
    out.push(0); // bitmap bit order: LSBFirst
    out.push(32); // bitmap format scanline unit
    out.push(32); // bitmap format scanline pad
    out.push(8); // min keycode
    out.push(255); // max keycode
    out.extend_from_slice(&[0, 0, 0, 0]); // unused

    // Vendor string padded.
    out.extend_from_slice(vendor);
    pad4(&mut out);

    // Pixmap formats.
    push_format(byte_order, 1, 1, 32, &mut out);   // depth 1, bpp 1
    push_format(byte_order, 24, 32, 32, &mut out); // depth 24, bpp 32
    push_format(byte_order, 32, 32, 32, &mut out); // depth 32, bpp 32

    // Screen.
    let root_window: u32 = 0x2000_0000;
    let default_visual: u32 = 0x21;
    let width_px: u16 = 800;
    let height_px: u16 = 600;
    let width_mm: u16 = 212; // ~800 / 96dpi * 25.4
    let height_mm: u16 = 159; // ~600 / 96dpi * 25.4

    byte_order.write_u32(root_window, &mut out); // root
    byte_order.write_u32(0, &mut out); // default colormap
    byte_order.write_u32(0xffffff, &mut out); // white pixel
    byte_order.write_u32(0x000000, &mut out); // black pixel
    byte_order.write_u32(0, &mut out); // current input masks
    byte_order.write_u16(width_px, &mut out); // width in pixels
    byte_order.write_u16(height_px, &mut out); // height in pixels
    byte_order.write_u16(width_mm, &mut out); // width in millimeters
    byte_order.write_u16(height_mm, &mut out); // height in millimeters
    byte_order.write_u16(1, &mut out); // min installed maps
    byte_order.write_u16(1, &mut out); // max installed maps
    byte_order.write_u32(default_visual, &mut out); // root visual
    out.push(1); // backing store: WhenMapped
    out.push(1); // save unders: true
    out.push(24); // root depth
    out.push(1); // number of allowed depths

    // Depth block.
    out.push(24); // depth value
    out.push(0); // pad
    byte_order.write_u16(1, &mut out); // number of visuals
    out.extend_from_slice(&[0, 0]); // pad to 4 bytes

    // Visual type.
    byte_order.write_u32(default_visual, &mut out); // visual ID
    out.push(4); // class TrueColor
    out.push(24); // bits per rgb value
    byte_order.write_u16(256, &mut out); // colormap entries
    byte_order.write_u32(0x00ff0000, &mut out); // red mask
    byte_order.write_u32(0x0000ff00, &mut out); // green mask
    byte_order.write_u32(0x000000ff, &mut out); // blue mask
    out.extend_from_slice(&[0, 0, 0, 0]); // pad

    // Align to 4-byte boundary before computing length.
    pad4(&mut out);

    // Fill in length.
    let length = ((out.len() - 8) / 4) as u16;
    let length_bytes = match byte_order {
        ByteOrder::LsbFirst => length.to_le_bytes(),
        ByteOrder::MsbFirst => length.to_be_bytes(),
    };
    out[6] = length_bytes[0];
    out[7] = length_bytes[1];

    out
}

fn push_format(_order: ByteOrder, depth: u8, bits_per_pixel: u8, scanline_pad: u8, out: &mut Vec<u8>) {
    out.push(depth);
    out.push(bits_per_pixel);
    out.push(scanline_pad);
    out.push(0); // pad
}

fn pad4(out: &mut Vec<u8>) {
    let pad = (4 - (out.len() % 4)) % 4;
    out.extend(std::iter::repeat(0).take(pad));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_setup_request_roundtrip() {
        // Little endian request with auth strings.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[
            b'l', 0, // byte order + pad
        ]);
        buf.extend_from_slice(&11u16.to_le_bytes()); // major
        buf.extend_from_slice(&0u16.to_le_bytes()); // minor
        buf.extend_from_slice(&4u16.to_le_bytes()); // auth name len
        buf.extend_from_slice(&3u16.to_le_bytes()); // auth data len
        buf.extend_from_slice(&[0, 0]); // pad
        buf.extend_from_slice(b"MIT1"); // already aligned, no pad
        buf.extend_from_slice(b"abc");
        buf.extend_from_slice(&[0]); // pad to 4

        let (req, used) = parse_setup_request(&buf).unwrap();
        assert_eq!(used, buf.len());
        assert_eq!(req.byte_order, ByteOrder::LsbFirst);
        assert_eq!(req.protocol_major, 11);
        assert_eq!(req.protocol_minor, 0);
        assert_eq!(req.auth_name, b"MIT1");
        assert_eq!(req.auth_data, b"abc");
    }

    #[test]
    fn build_setup_success_length_matches() {
        let reply = build_setup_success(ByteOrder::LsbFirst);
        // Length field equals (total - 8) / 4
        let len_field = u16::from_le_bytes([reply[6], reply[7]]);
        assert_eq!(len_field as usize * 4 + 8, reply.len());
        assert_eq!(reply[0], 1);
    }
}
