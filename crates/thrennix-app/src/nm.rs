//! Native Messaging framing: u32 little-endian length followed by UTF-8 JSON.

use std::io::{self, Read, Write};

/// Largest message accepted in either direction.
pub const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Message(Vec<u8>),
    /// The frame was larger than [`MAX_MESSAGE`]; its body has been skipped.
    TooLarge(u32),
}

pub fn encode(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + msg.len());
    out.extend_from_slice(&(msg.len() as u32).to_le_bytes());
    out.extend_from_slice(msg);
    out
}

/// Reads one frame. `Ok(None)` on a clean EOF before the length prefix.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_le_bytes(len);
    if len as usize > MAX_MESSAGE {
        let skipped = io::copy(&mut r.take(len as u64), &mut io::sink())?;
        if skipped < len as u64 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        return Ok(Some(Frame::TooLarge(len)));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(Frame::Message(buf)))
}

pub fn write_frame<W: Write>(w: &mut W, msg: &[u8]) -> io::Result<()> {
    if msg.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message too large",
        ));
    }
    w.write_all(&encode(msg))?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn encode_layout() {
        assert_eq!(encode(b"{}"), vec![2, 0, 0, 0, b'{', b'}']);
        assert_eq!(encode(b""), vec![0, 0, 0, 0]);
    }

    #[test]
    fn roundtrip_multiple() {
        let mut data = Vec::new();
        write_frame(&mut data, br#"{"type":"ping"}"#).unwrap();
        write_frame(&mut data, "{\"a\":\"\u{e9}\"}".as_bytes()).unwrap();
        let mut c = Cursor::new(data);
        assert_eq!(
            read_frame(&mut c).unwrap(),
            Some(Frame::Message(br#"{"type":"ping"}"#.to_vec()))
        );
        assert_eq!(
            read_frame(&mut c).unwrap(),
            Some(Frame::Message("{\"a\":\"\u{e9}\"}".as_bytes().to_vec()))
        );
        assert_eq!(read_frame(&mut c).unwrap(), None);
    }

    #[test]
    fn truncated() {
        let mut c = Cursor::new(vec![10, 0, 0, 0, b'{']);
        assert!(read_frame(&mut c).is_err());
        let mut c = Cursor::new(vec![10, 0]);
        assert!(read_frame(&mut c).is_err());
    }

    #[test]
    fn too_large_is_skipped() {
        let big = MAX_MESSAGE + 1;
        let mut data = (big as u32).to_le_bytes().to_vec();
        data.extend(std::iter::repeat_n(b' ', big));
        data.extend(encode(b"{}"));
        let mut c = Cursor::new(data);
        assert_eq!(
            read_frame(&mut c).unwrap(),
            Some(Frame::TooLarge(big as u32))
        );
        assert_eq!(
            read_frame(&mut c).unwrap(),
            Some(Frame::Message(b"{}".to_vec()))
        );
        assert!(write_frame(&mut Vec::new(), &vec![b'x'; big]).is_err());
    }
}
