//! Shell DLL <-> worker wire protocol over anonymous pipes (stdin/stdout), REQ-DEC-1.
//!
//! Request:  `TWQ2` | decoder u8 | edge u32 | param u8 | ext_len u8 | ext | data_len u64 | data
//! Response: `TWR1` | status u8 | (status 0) width u32 | height u32 | BGRA bytes
//!                               | (status != 0) msg_len u16 | msg
//! All integers little-endian. Pixels are premultiplied BGRA, top-down.
//!
//! The parent treats the worker as hostile: responses are size-checked against the
//! requested edge before any allocation.

use std::io::{self, Read, Write};

use crate::decode::route::Decoder;
use crate::decode::{DecodeError, Thumbnail};

const REQ_MAGIC: &[u8; 4] = b"TWQ2";
const RESP_MAGIC: &[u8; 4] = b"TWR1";

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub decoder: Decoder,
    pub edge: u32,
    /// Generic one-byte decoder parameter (video: frame offset in percent).
    pub param: u8,
    pub ext: String,
    pub data: Vec<u8>,
}

pub fn write_request(
    w: &mut impl Write,
    decoder: Decoder,
    edge: u32,
    param: u8,
    ext: &str,
    data: &[u8],
) -> io::Result<()> {
    let ext = ext.as_bytes();
    let ext = &ext[..ext.len().min(u8::MAX as usize)];
    w.write_all(REQ_MAGIC)?;
    w.write_all(&[decoder.code()])?;
    w.write_all(&edge.to_le_bytes())?;
    w.write_all(&[param])?;
    w.write_all(&[ext.len() as u8])?;
    w.write_all(ext)?;
    w.write_all(&(data.len() as u64).to_le_bytes())?;
    w.write_all(data)?;
    w.flush()
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn read_array<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

/// Reads one request; `max_data` bounds the payload allocation.
pub fn read_request(r: &mut impl Read, max_data: u64) -> io::Result<Request> {
    if &read_array::<4>(r)? != REQ_MAGIC {
        return Err(invalid("bad request magic"));
    }
    let [code] = read_array::<1>(r)?;
    let decoder = Decoder::from_code(code).ok_or_else(|| invalid("unknown decoder"))?;
    let edge = u32::from_le_bytes(read_array(r)?);
    let [param] = read_array::<1>(r)?;
    let [ext_len] = read_array::<1>(r)?;
    let mut ext = vec![0u8; ext_len as usize];
    r.read_exact(&mut ext)?;
    let ext = String::from_utf8(ext).map_err(|_| invalid("ext not UTF-8"))?;
    let len = u64::from_le_bytes(read_array(r)?);
    if len > max_data {
        return Err(invalid("payload exceeds limit"));
    }
    let mut data = Vec::with_capacity(len as usize);
    r.take(len).read_to_end(&mut data)?;
    if data.len() as u64 != len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Request {
        decoder,
        edge,
        param,
        ext,
        data,
    })
}

fn status_code(e: &DecodeError) -> u8 {
    match e {
        DecodeError::Unsupported => 1,
        DecodeError::Malformed(_) => 2,
        DecodeError::TooLarge => 3,
        DecodeError::Timeout => 4,
    }
}

pub fn write_response(
    w: &mut impl Write,
    result: &Result<Thumbnail, DecodeError>,
) -> io::Result<()> {
    w.write_all(RESP_MAGIC)?;
    match result {
        Ok(t) => {
            w.write_all(&[0])?;
            w.write_all(&t.width.to_le_bytes())?;
            w.write_all(&t.height.to_le_bytes())?;
            w.write_all(&t.bgra)?;
        }
        Err(e) => {
            w.write_all(&[status_code(e)])?;
            let msg = match e {
                DecodeError::Malformed(m) => m.as_bytes(),
                _ => &[],
            };
            let msg = &msg[..msg.len().min(1024)];
            w.write_all(&(msg.len() as u16).to_le_bytes())?;
            w.write_all(msg)?;
        }
    }
    w.flush()
}

/// Reads and validates a response. Dimensions above `max_edge` are rejected before the
/// pixel buffer is allocated. I/O errors (worker died, pipe closed) surface as `Err(io)`.
pub fn read_response(
    r: &mut impl Read,
    max_edge: u32,
) -> io::Result<Result<Thumbnail, DecodeError>> {
    if &read_array::<4>(r)? != RESP_MAGIC {
        return Err(invalid("bad response magic"));
    }
    let [status] = read_array::<1>(r)?;
    if status == 0 {
        let width = u32::from_le_bytes(read_array(r)?);
        let height = u32::from_le_bytes(read_array(r)?);
        if width == 0 || height == 0 || width > max_edge || height > max_edge {
            return Err(invalid("thumbnail dimensions out of range"));
        }
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        r.read_exact(&mut bgra)?;
        return Ok(Ok(Thumbnail {
            width,
            height,
            bgra,
        }));
    }
    let len = u16::from_le_bytes(read_array(r)?);
    let mut msg = vec![0u8; len as usize];
    r.read_exact(&mut msg)?;
    Ok(Err(match status {
        1 => DecodeError::Unsupported,
        2 => DecodeError::Malformed(String::from_utf8_lossy(&msg).into_owned()),
        3 => DecodeError::TooLarge,
        4 => DecodeError::Timeout,
        _ => return Err(invalid("unknown status")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_and_payload_cap() {
        let mut buf = Vec::new();
        write_request(&mut buf, Decoder::Svg, 256, 30, "svg", b"<svg/>").unwrap();
        let req = read_request(&mut buf.as_slice(), 1024).unwrap();
        assert_eq!(
            req,
            Request {
                decoder: Decoder::Svg,
                edge: 256,
                param: 30,
                ext: "svg".into(),
                data: b"<svg/>".to_vec()
            }
        );
        assert!(read_request(&mut buf.as_slice(), 3).is_err());
    }

    #[test]
    fn request_param_survives_every_value_and_old_magic_is_rejected() {
        for param in [0u8, 1, 30, 100, 255] {
            let mut buf = Vec::new();
            write_request(&mut buf, Decoder::Video, 128, param, "mp4", b"x").unwrap();
            assert_eq!(read_request(&mut buf.as_slice(), 16).unwrap().param, param);
        }
        // A request from a pre-`param` build (TWQ1) must not be parsed with shifted fields.
        let mut old = Vec::new();
        write_request(&mut old, Decoder::Svg, 64, 0, "svg", b"<svg/>").unwrap();
        old[3] = b'1';
        assert!(read_request(&mut old.as_slice(), 16).is_err());
        // Truncated right after the param byte.
        let mut buf = Vec::new();
        write_request(&mut buf, Decoder::Video, 128, 30, "mp4", b"x").unwrap();
        assert!(read_request(&mut &buf[..10], 16).is_err());
    }

    #[test]
    fn response_round_trip() {
        let t = Thumbnail {
            width: 2,
            height: 1,
            bgra: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &Ok(t.clone())).unwrap();
        assert_eq!(read_response(&mut buf.as_slice(), 2).unwrap(), Ok(t));

        let mut buf = Vec::new();
        write_response(&mut buf, &Err(DecodeError::malformed("bad"))).unwrap();
        assert_eq!(
            read_response(&mut buf.as_slice(), 2).unwrap(),
            Err(DecodeError::Malformed("bad".into()))
        );
    }

    #[test]
    fn oversized_or_truncated_response_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(RESP_MAGIC);
        buf.push(0);
        buf.extend_from_slice(&100_000u32.to_le_bytes());
        buf.extend_from_slice(&100_000u32.to_le_bytes());
        assert!(read_response(&mut buf.as_slice(), 256).is_err());

        let t = Thumbnail {
            width: 4,
            height: 4,
            bgra: vec![0; 64],
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &Ok(t)).unwrap();
        buf.truncate(buf.len() - 1);
        assert!(read_response(&mut buf.as_slice(), 256).is_err());
    }
}
