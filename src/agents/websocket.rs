//! A minimal RFC 6455 client, just enough to talk JSON-RPC to the Codex
//! app-server over its owner-private Unix socket. Text messages only, no
//! extensions. The peer is verified by socket ownership before connecting, so
//! the `Sec-WebSocket-Accept` hash is not recomputed.

use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const MAX_HEADER: usize = 16 * 1024;
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub struct WebSocket<S: Read + Write> {
    stream: S,
}

impl<S: Read + Write> WebSocket<S> {
    pub fn handshake(mut stream: S) -> io::Result<Self> {
        let key = base64(&nonce());
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes())?;
        // Byte by byte, so no frame bytes behind the headers are swallowed.
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > MAX_HEADER {
                return Err(invalid("handshake response too large"));
            }
            stream.read_exact(&mut byte)?;
            head.push(byte[0]);
        }
        let status = head.split(|b| *b == b'\r').next().unwrap_or_default();
        if !String::from_utf8_lossy(status).contains(" 101 ") {
            return Err(invalid("websocket upgrade refused"));
        }
        Ok(Self { stream })
    }

    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.stream
            .write_all(&encode_frame(0x1, text.as_bytes(), mask_key()))?;
        self.stream.flush()
    }

    /// The next complete text message. Pings are answered, other control
    /// frames are skipped, and a close frame ends the stream.
    pub fn read_text(&mut self) -> io::Result<String> {
        let mut message: Vec<u8> = Vec::new();
        loop {
            let frame = read_frame(&mut self.stream)?;
            match frame.opcode {
                0x0..=0x2 => {
                    if message.len() + frame.payload.len() > MAX_MESSAGE {
                        return Err(invalid("message too large"));
                    }
                    message.extend_from_slice(&frame.payload);
                    if frame.fin {
                        return String::from_utf8(message).map_err(|_| invalid("not utf-8"));
                    }
                }
                0x8 => return Err(io::ErrorKind::UnexpectedEof.into()),
                0x9 => {
                    self.stream
                        .write_all(&encode_frame(0xA, &frame.payload, mask_key()))?;
                    self.stream.flush()?;
                }
                _ => {}
            }
        }
    }
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    match payload.len() {
        len if len < 126 => out.push(0x80 | len as u8),
        len if len <= usize::from(u16::MAX) => {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

fn read_frame(stream: &mut impl Read) -> io::Result<Frame> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;
    let len = match head[1] & 0x7F {
        126 => {
            let mut raw = [0u8; 2];
            stream.read_exact(&mut raw)?;
            u64::from(u16::from_be_bytes(raw))
        }
        127 => {
            let mut raw = [0u8; 8];
            stream.read_exact(&mut raw)?;
            u64::from_be_bytes(raw)
        }
        short => u64::from(short),
    };
    if len > MAX_MESSAGE as u64 {
        return Err(invalid("frame too large"));
    }
    let mut mask = [0u8; 4];
    let masked = head[1] & 0x80 != 0;
    if masked {
        stream.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload)?;
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    Ok(Frame {
        fin: head[0] & 0x80 != 0,
        opcode: head[0] & 0x0F,
        payload,
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Frame masking only has to be unpredictable to intermediaries; the clock is
/// enough for a local socket.
fn mask_key() -> [u8; 4] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0x5eed);
    nanos.wrapping_mul(2_654_435_761).to_be_bytes()
}

fn nonce() -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, chunk) in out.chunks_mut(4).enumerate() {
        chunk.copy_from_slice(&mask_key().map(|b| b.wrapping_add(i as u8 * 37)));
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Duplex {
        incoming: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.incoming.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
        if payload.len() < 126 {
            out.push(payload.len() as u8);
        } else {
            out.push(126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    fn connected(frames: Vec<u8>) -> WebSocket<Duplex> {
        let mut data = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n".to_vec();
        data.extend(frames);
        WebSocket::handshake(Duplex {
            incoming: Cursor::new(data),
            written: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn client_frames_are_masked_and_round_trip() {
        let frame = encode_frame(0x1, b"hello", [1, 2, 3, 4]);
        assert_eq!(frame[1], 0x80 | 5);
        let decoded = read_frame(&mut Cursor::new(frame)).unwrap();
        assert!(decoded.fin);
        assert_eq!(decoded.opcode, 0x1);
        assert_eq!(decoded.payload, b"hello");
    }

    #[test]
    fn long_payloads_use_the_extended_length() {
        let payload = vec![b'x'; 300];
        let decoded =
            read_frame(&mut Cursor::new(encode_frame(0x1, &payload, [9, 8, 7, 6]))).unwrap();
        assert_eq!(decoded.payload.len(), 300);
    }

    #[test]
    fn fragments_are_joined_and_pings_are_answered() {
        let mut data = server_frame(false, 0x1, b"{\"a\":");
        data.extend(server_frame(true, 0x9, b"p"));
        data.extend(server_frame(true, 0x0, b"1}"));
        let mut socket = connected(data);

        assert_eq!(socket.read_text().unwrap(), "{\"a\":1}");
        let written = &socket.stream.written;
        assert!(
            written.windows(1).any(|w| w[0] == 0x80 | 0xA),
            "no pong sent"
        );
    }

    #[test]
    fn a_close_frame_ends_the_stream_and_a_refused_upgrade_fails() {
        let mut socket = connected(server_frame(true, 0x8, b""));
        assert!(socket.read_text().is_err());

        let refused = WebSocket::handshake(Duplex {
            incoming: Cursor::new(b"HTTP/1.1 403 Forbidden\r\n\r\n".to_vec()),
            written: Vec::new(),
        });
        assert!(refused.is_err());
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut data = vec![0x81, 127];
        data.extend_from_slice(&(u64::MAX).to_be_bytes());
        assert!(read_frame(&mut Cursor::new(data)).is_err());
    }
}
