//! Framed wire protocol, shared by the daemon and every client.
//!
//! Frames travel over any ordered byte stream (unix socket, ssh pipe).
//! Header is 8 bytes little-endian: payload length (u32, capped),
//! frame type (u8), flags (u8, reserved 0), reserved u16 (0).
//!
//! Version negotiation happens in the hello exchange; v1 requires an
//! exact match. Contract: only [`FrameType::Exited`] means the shell
//! died — transport EOF without it means the link was lost.

use std::io;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_PAYLOAD: usize = 64 * 1024;
pub const HEADER_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub len: u32,
    pub typ: u8,
    pub flags: u8,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    // client -> server
    ClientHello = 0x01,
    Stdin = 0x02,
    Resize = 0x03,
    Kill = 0x04,
    Detach = 0x05,
    Ping = 0x06,
    // server -> client
    ServerHello = 0x81,
    Output = 0x82,
    ReplayBegin = 0x83,
    ReplayEnd = 0x84,
    Exited = 0x85,
    Detached = 0x86,
    Error = 0x87,
    Pong = 0x88,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<FrameType> {
        Some(match v {
            0x01 => FrameType::ClientHello,
            0x02 => FrameType::Stdin,
            0x03 => FrameType::Resize,
            0x04 => FrameType::Kill,
            0x05 => FrameType::Detach,
            0x06 => FrameType::Ping,
            0x81 => FrameType::ServerHello,
            0x82 => FrameType::Output,
            0x83 => FrameType::ReplayBegin,
            0x84 => FrameType::ReplayEnd,
            0x85 => FrameType::Exited,
            0x86 => FrameType::Detached,
            0x87 => FrameType::Error,
            0x88 => FrameType::Pong,
            _ => return None,
        })
    }
}

/// Reasons carried by [`FrameType::Detached`].
pub const DETACH_SERVER_SHUTDOWN: u8 = 0;
pub const DETACH_REPLACED: u8 = 1;
pub const DETACH_SLOW_CONSUMER: u8 = 2;

/// Error codes carried by [`FrameType::Error`].
pub const ERROR_VERSION_MISMATCH: u8 = 1;

#[derive(Debug)]
pub enum ProtocolError {
    Oversize(u32),
    UnknownType(u8),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Oversize(n) => write!(f, "frame payload {n} exceeds cap"),
            ProtocolError::UnknownType(t) => write!(f, "unknown frame type {t:#04x}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Incremental frame decoder. Feed arbitrary byte chunks; pop complete
/// frames. Never copies payloads twice: the internal buffer is drained
/// front-to-back per frame.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    off: usize,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // Compact lazily: only when the dead prefix dominates.
        if self.off > 0 && self.off >= self.buf.len() / 2 {
            self.buf.drain(..self.off);
            self.off = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Unconsumed bytes buffered. Callers that feed in a loop use this
    /// as a defense-in-depth ceiling: a peer streaming faster than
    /// frames are drained must be disconnected, not buffered forever.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.off
    }

    /// `Ok(None)` = need more bytes. `Err` = protocol violation; the
    /// stream is unrecoverable and must be closed.
    pub fn next_frame(
        &mut self,
    ) -> Result<Option<(FrameHeader, Vec<u8>)>, ProtocolError> {
        let avail = self.buf.len() - self.off;
        if avail < HEADER_LEN {
            return Ok(None);
        }
        let h = &self.buf[self.off..self.off + HEADER_LEN];
        let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let typ = h[4];
        let flags = h[5];
        if len as usize > MAX_PAYLOAD {
            return Err(ProtocolError::Oversize(len));
        }
        if FrameType::from_u8(typ).is_none() {
            return Err(ProtocolError::UnknownType(typ));
        }
        if avail < HEADER_LEN + len as usize {
            return Ok(None);
        }
        let start = self.off + HEADER_LEN;
        let payload = self.buf[start..start + len as usize].to_vec();
        self.off = start + len as usize;
        Ok(Some((FrameHeader { len, typ, flags }, payload)))
    }
}

pub fn write_frame(
    w: &mut impl io::Write,
    typ: FrameType,
    payload: &[u8],
) -> io::Result<()> {
    // Hard error, not debug_assert: a release build silently emitting
    // an oversize frame just moves the failure to the receiver, which
    // rejects it as a protocol violation and drops the link.
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::other("frame payload exceeds MAX_PAYLOAD"));
    }
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[4] = typ as u8;
    w.write_all(&header)?;
    w.write_all(payload)
}

/// Feature bits carried in `ClientHello`.
pub const FEATURE_NO_REPLAY: u32 = 1;

/// `ClientHello` payload: proto version + feature bits.
pub fn encode_client_hello() -> Vec<u8> {
    encode_client_hello_with(0)
}

pub fn encode_client_hello_with(features: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(8);
    p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    p.extend_from_slice(&features.to_le_bytes());
    p
}

pub fn decode_client_hello(payload: &[u8]) -> Option<u32> {
    payload
        .get(..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Feature bits from a `ClientHello` payload (0 when absent).
pub fn decode_client_hello_features(payload: &[u8]) -> u32 {
    payload
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}

pub const SHELL_RUNNING: u8 = 0;
pub const SHELL_EXITED: u8 = 1;

/// `ServerHello` payload: version, daemon pid, shell pid, shell state,
/// exit status, then JSON metadata (pane_id/program/cwd).
pub struct ServerHello {
    pub version: u32,
    pub daemon_pid: u32,
    pub shell_pid: u32,
    pub shell_state: u8,
    pub exit_status: i32,
    pub meta_json: String,
}

impl ServerHello {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(17 + self.meta_json.len());
        p.extend_from_slice(&self.version.to_le_bytes());
        p.extend_from_slice(&self.daemon_pid.to_le_bytes());
        p.extend_from_slice(&self.shell_pid.to_le_bytes());
        p.push(self.shell_state);
        p.extend_from_slice(&self.exit_status.to_le_bytes());
        p.extend_from_slice(self.meta_json.as_bytes());
        p
    }

    pub fn decode(payload: &[u8]) -> Option<ServerHello> {
        if payload.len() < 17 {
            return None;
        }
        let u = |i: usize| {
            u32::from_le_bytes([
                payload[i],
                payload[i + 1],
                payload[i + 2],
                payload[i + 3],
            ])
        };
        Some(ServerHello {
            version: u(0),
            daemon_pid: u(4),
            shell_pid: u(8),
            shell_state: payload[12],
            exit_status: i32::from_le_bytes([
                payload[13],
                payload[14],
                payload[15],
                payload[16],
            ]),
            meta_json: String::from_utf8_lossy(&payload[17..]).into_owned(),
        })
    }
}

/// `Resize` payload: rows, cols, xpixel, ypixel (u16 LE each).
pub fn encode_resize(rows: u16, cols: u16, xpixel: u16, ypixel: u16) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0..2].copy_from_slice(&rows.to_le_bytes());
    p[2..4].copy_from_slice(&cols.to_le_bytes());
    p[4..6].copy_from_slice(&xpixel.to_le_bytes());
    p[6..8].copy_from_slice(&ypixel.to_le_bytes());
    p
}

pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16, u16, u16)> {
    if payload.len() < 8 {
        return None;
    }
    let g = |i: usize| u16::from_le_bytes([payload[i], payload[i + 1]]);
    Some((g(0), g(2), g(4), g(6)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_frames() {
        let mut wire = Vec::new();
        write_frame(&mut wire, FrameType::Stdin, b"hello").unwrap();
        write_frame(&mut wire, FrameType::Ping, &[]).unwrap();
        write_frame(&mut wire, FrameType::Output, &[0u8; MAX_PAYLOAD]).unwrap();

        let mut dec = Decoder::new();
        // Feed in awkward chunk sizes to exercise partial-frame paths.
        for chunk in wire.chunks(7) {
            dec.feed(chunk);
        }
        let (h1, p1) = dec.next_frame().unwrap().unwrap();
        assert_eq!(FrameType::from_u8(h1.typ), Some(FrameType::Stdin));
        assert_eq!(p1, b"hello");
        let (h2, p2) = dec.next_frame().unwrap().unwrap();
        assert_eq!(FrameType::from_u8(h2.typ), Some(FrameType::Ping));
        assert!(p2.is_empty());
        let (h3, p3) = dec.next_frame().unwrap().unwrap();
        assert_eq!(FrameType::from_u8(h3.typ), Some(FrameType::Output));
        assert_eq!(p3.len(), MAX_PAYLOAD);
        assert!(dec.next_frame().unwrap().is_none());
    }

    #[test]
    fn partial_header_then_completion() {
        let mut wire = Vec::new();
        write_frame(&mut wire, FrameType::Stdin, b"abc").unwrap();
        let mut dec = Decoder::new();
        dec.feed(&wire[..3]);
        assert!(dec.next_frame().unwrap().is_none());
        dec.feed(&wire[3..9]);
        assert!(dec.next_frame().unwrap().is_none());
        dec.feed(&wire[9..]);
        let (_, p) = dec.next_frame().unwrap().unwrap();
        assert_eq!(p, b"abc");
    }

    #[test]
    fn oversize_and_unknown_type_are_fatal() {
        let mut dec = Decoder::new();
        let mut bad = Vec::new();
        bad.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        bad.extend_from_slice(&[FrameType::Stdin as u8, 0, 0, 0]);
        dec.feed(&bad);
        assert!(matches!(dec.next_frame(), Err(ProtocolError::Oversize(_))));

        let mut dec = Decoder::new();
        let mut bad = Vec::new();
        bad.extend_from_slice(&0u32.to_le_bytes());
        bad.extend_from_slice(&[0x7F, 0, 0, 0]);
        dec.feed(&bad);
        assert!(matches!(
            dec.next_frame(),
            Err(ProtocolError::UnknownType(0x7F))
        ));
    }

    #[test]
    fn hello_and_resize_round_trip() {
        let sh = ServerHello {
            version: PROTOCOL_VERSION,
            daemon_pid: 42,
            shell_pid: 43,
            shell_state: SHELL_RUNNING,
            exit_status: 0,
            meta_json: r#"{"pane_id":"ab"}"#.into(),
        };
        let d = ServerHello::decode(&sh.encode()).unwrap();
        assert_eq!(d.daemon_pid, 42);
        assert_eq!(d.shell_pid, 43);
        assert_eq!(d.meta_json, r#"{"pane_id":"ab"}"#);

        let r = encode_resize(50, 120, 800, 600);
        assert_eq!(decode_resize(&r), Some((50, 120, 800, 600)));
    }
}
