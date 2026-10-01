//! Length-prefixed JSON framing for the IPC pipe: a `u32` little-endian byte
//! length followed by the JSON body. Generic over any `Read`/`Write`, so it is
//! fully unit-testable on an in-memory buffer without a real pipe.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Cap on a request frame (client → service). Requests are tiny (a query plus
/// flags), so keep this small — the service runs as LocalSystem and accepts any
/// authenticated user, so a generous cap would be a memory-amplification lever.
pub const MAX_REQUEST: u32 = 64 * 1024;

/// Cap on a response frame (service → client). Larger to fit big result sets,
/// but still bounded against a runaway/incompatible peer.
pub const MAX_RESPONSE: u32 = 256 * 1024 * 1024;

/// Serialize `msg` as a length-prefixed JSON frame and write it.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame, rejecting any frame larger than `max`.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R, max: u32) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > max {
        return Err(io::Error::other(format!(
            "frame of {len} bytes exceeds the {max}-byte limit"
        )));
    }
    // Grow with the bytes actually received, not the (untrusted) declared length,
    // so a hostile prefix can't force a large up-front allocation.
    let mut body = Vec::new();
    let read = r.take(len as u64).read_to_end(&mut body)?;
    if read != len as usize {
        return Err(io::Error::other("connection closed mid-frame"));
    }
    serde_json::from_slice(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ipc::{Request, Response, PROTOCOL_VERSION};

    #[test]
    fn frame_roundtrip_over_buffer() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let back: Request = read_frame(&mut cur, MAX_REQUEST).unwrap();
        assert!(matches!(back, Request::Ping));
    }

    #[test]
    fn two_frames_back_to_back() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Request::Status).unwrap();
        write_frame(
            &mut buf,
            &Response::Pong {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let a: Request = read_frame(&mut cur, MAX_REQUEST).unwrap();
        let b: Response = read_frame(&mut cur, MAX_RESPONSE).unwrap();
        assert!(matches!(a, Request::Status));
        assert!(matches!(b, Response::Pong { .. }));
    }

    #[test]
    fn oversized_frame_is_rejected() {
        // A length prefix beyond the cap must error rather than allocate.
        let mut bytes = 64u32.to_le_bytes().to_vec(); // claims 64 bytes
        bytes.extend_from_slice(&[0u8; 64]);
        let mut cur = std::io::Cursor::new(bytes);
        let err = read_frame::<_, Request>(&mut cur, 16).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }
}
