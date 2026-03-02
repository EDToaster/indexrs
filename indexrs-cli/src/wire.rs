//! TLV (Tag-Length-Value) binary framing for daemon responses.
//!
//! Every response frame is: `[tag: u8][len: u32 LE][payload: len bytes]`
//!
//! | Tag  | Variant    | Payload                                              |
//! |------|-----------|------------------------------------------------------|
//! | 0x01 | Line     | Raw UTF-8 string                                     |
//! | 0x02 | Done     | Fixed 17 bytes: total:u64 LE + duration_ms:u64 LE + stale:u8 |
//! | 0x03 | Error    | Raw UTF-8 string                                     |
//! | 0x04 | Pong     | Empty (len=0)                                        |
//! | 0x05 | Progress | Raw UTF-8 string                                     |

use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::daemon::DaemonResponse;

pub(crate) const TAG_LINE: u8 = 0x01;
const TAG_DONE: u8 = 0x02;
const TAG_ERROR: u8 = 0x03;
const TAG_PONG: u8 = 0x04;
const TAG_PROGRESS: u8 = 0x05;

const DONE_PAYLOAD_LEN: u32 = 17;

/// Reject string payloads larger than 64 MB (protects against corrupted frames).
const MAX_STRING_PAYLOAD: u32 = 64 * 1024 * 1024;

/// Write a string-payload frame: `[tag][len: u32 LE][utf8 bytes]`.
async fn write_string_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    tag: u8,
    s: &str,
) -> io::Result<()> {
    let payload = s.as_bytes();
    let mut header = [0u8; 5];
    header[0] = tag;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await
}

/// Read `len` bytes from the reader and convert to a UTF-8 string.
async fn read_utf8<R: AsyncReadExt + Unpin>(reader: &mut R, len: u32) -> io::Result<String> {
    if len > MAX_STRING_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("string payload too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Write a `DaemonResponse` as a TLV frame.
pub async fn write_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &DaemonResponse,
) -> io::Result<()> {
    match resp {
        DaemonResponse::Line { content } => write_string_frame(writer, TAG_LINE, content).await,
        DaemonResponse::Done {
            total,
            duration_ms,
            stale,
        } => {
            // Write all 5 header + 17 payload bytes as a single array.
            let mut buf = [0u8; 5 + DONE_PAYLOAD_LEN as usize];
            buf[0] = TAG_DONE;
            buf[1..5].copy_from_slice(&DONE_PAYLOAD_LEN.to_le_bytes());
            buf[5..13].copy_from_slice(&(*total as u64).to_le_bytes());
            buf[13..21].copy_from_slice(&duration_ms.to_le_bytes());
            buf[21] = u8::from(*stale);
            writer.write_all(&buf).await
        }
        DaemonResponse::Error { message } => write_string_frame(writer, TAG_ERROR, message).await,
        DaemonResponse::Pong => writer.write_all(&[TAG_PONG, 0, 0, 0, 0]).await,
        DaemonResponse::Progress { message } => {
            write_string_frame(writer, TAG_PROGRESS, message).await
        }
    }
}

/// Read a `DaemonResponse` from a TLV frame.
pub async fn read_response<R: AsyncReadExt + Unpin>(reader: &mut R) -> io::Result<DaemonResponse> {
    let tag = reader.read_u8().await?;
    let len = reader.read_u32_le().await?;

    match tag {
        TAG_LINE => {
            let content = read_utf8(reader, len).await?;
            Ok(DaemonResponse::Line { content })
        }
        TAG_DONE => {
            if len != DONE_PAYLOAD_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Done payload must be {DONE_PAYLOAD_LEN} bytes, got {len}"),
                ));
            }
            let total = reader.read_u64_le().await? as usize;
            let duration_ms = reader.read_u64_le().await?;
            let stale_byte = reader.read_u8().await?;
            Ok(DaemonResponse::Done {
                total,
                duration_ms,
                stale: stale_byte != 0,
            })
        }
        TAG_ERROR => {
            let message = read_utf8(reader, len).await?;
            Ok(DaemonResponse::Error { message })
        }
        TAG_PONG => {
            if len != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Pong payload must be 0 bytes, got {len}"),
                ));
            }
            Ok(DaemonResponse::Pong)
        }
        TAG_PROGRESS => {
            let message = read_utf8(reader, len).await?;
            Ok(DaemonResponse::Progress { message })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown response tag: 0x{tag:02X}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// Helper: write a response to a Vec, then read it back via Cursor.
    async fn roundtrip(resp: &DaemonResponse) -> DaemonResponse {
        let mut buf = Vec::new();
        write_response(&mut buf, resp).await.unwrap();
        let mut cursor = Cursor::new(buf);
        read_response(&mut cursor).await.unwrap()
    }

    #[tokio::test]
    async fn test_roundtrip_line() {
        let resp = DaemonResponse::Line {
            content: "hello world".to_string(),
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_line_with_special_chars() {
        let resp = DaemonResponse::Line {
            content: r#"path "with" quotes \ and \n escapes"#.to_string(),
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_line_with_ansi_codes() {
        let resp = DaemonResponse::Line {
            content: "\x1b[31mred\x1b[0m normal \x1b[1;32mbold green\x1b[0m".to_string(),
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_done() {
        let resp = DaemonResponse::Done {
            total: 42,
            duration_ms: 12345,
            stale: false,
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_done_stale() {
        let resp = DaemonResponse::Done {
            total: 1_000_000,
            duration_ms: 999,
            stale: true,
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_error() {
        let resp = DaemonResponse::Error {
            message: "something went wrong".to_string(),
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_pong() {
        let resp = DaemonResponse::Pong;
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_progress() {
        let resp = DaemonResponse::Progress {
            message: "indexing 50%".to_string(),
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_roundtrip_empty_line() {
        let resp = DaemonResponse::Line {
            content: String::new(),
        };
        assert_eq!(roundtrip(&resp).await, resp);
    }

    #[tokio::test]
    async fn test_unknown_tag_returns_error() {
        let buf: Vec<u8> = vec![0xFF, 0, 0, 0, 0];
        let mut cursor = Cursor::new(buf);
        let err = read_response(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("0xFF"));
    }

    #[tokio::test]
    async fn test_multiple_frames_sequential() {
        let frames = vec![
            DaemonResponse::Line {
                content: "first".to_string(),
            },
            DaemonResponse::Progress {
                message: "working".to_string(),
            },
            DaemonResponse::Done {
                total: 1,
                duration_ms: 100,
                stale: false,
            },
        ];

        let mut buf = Vec::new();
        for f in &frames {
            write_response(&mut buf, f).await.unwrap();
        }

        let mut cursor = Cursor::new(buf);
        for expected in &frames {
            let got = read_response(&mut cursor).await.unwrap();
            assert_eq!(&got, expected);
        }
    }

    #[tokio::test]
    async fn test_eof_returns_unexpected_eof() {
        let buf: Vec<u8> = Vec::new();
        let mut cursor = Cursor::new(buf);
        let err = read_response(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn test_line_frame_binary_layout() {
        let resp = DaemonResponse::Line {
            content: "abc".to_string(),
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).await.unwrap();

        // tag=0x01, len=3 (LE), payload="abc"
        assert_eq!(buf, vec![0x01, 0x03, 0x00, 0x00, 0x00, b'a', b'b', b'c']);
    }

    #[tokio::test]
    async fn test_done_frame_binary_layout() {
        let resp = DaemonResponse::Done {
            total: 42,
            duration_ms: 12345,
            stale: false,
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).await.unwrap();

        // Header: tag=0x02, len=17 LE
        assert_eq!(buf[0], 0x02);
        assert_eq!(&buf[1..5], &17u32.to_le_bytes());
        // Payload: total=42 u64 LE, duration_ms=12345 u64 LE, stale=0
        assert_eq!(&buf[5..13], &42u64.to_le_bytes());
        assert_eq!(&buf[13..21], &12345u64.to_le_bytes());
        assert_eq!(buf[21], 0x00);
        // Total frame size: 5 header + 17 payload = 22
        assert_eq!(buf.len(), 22);
    }

    #[tokio::test]
    async fn test_pong_nonzero_len_returns_error() {
        let buf: Vec<u8> = vec![TAG_PONG, 0x05, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0];
        let mut cursor = Cursor::new(buf);
        let err = read_response(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_pong_frame_binary_layout() {
        let resp = DaemonResponse::Pong;
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).await.unwrap();

        // tag=0x04, len=0, total 5 bytes
        assert_eq!(buf, vec![0x04, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(buf.len(), 5);
    }
}
