//! kDesk wire protocol.
//!
//! One TCP connection carries everything (the host listens, the phone connects
//! through `adb reverse`). Every message is a frame:
//!
//! ```text
//! +--------+----------------+-----------------+
//! | kind   | payload length | payload         |
//! | u8     | u32 big-endian | `length` bytes  |
//! +--------+----------------+-----------------+
//! ```
//!
//! All integers inside payloads are big-endian. The Android client mirrors
//! this layout byte for byte, so keep it boring.
//!
//! | kind | name   | dir          | payload                                              |
//! |------|--------|--------------|------------------------------------------------------|
//! | 0x01 | Hello  | client->host | version u16, width u32, height u32, name (rest, utf8) |
//! | 0x02 | Config | host->client | version u16, width u32, height u32, codec u8          |
//! | 0x03 | Video  | host->client | pts_us u64, flags u8, Annex-B H.264 (rest)            |
//! | 0x10 | Touch  | client->host | action u8, pointer_id u8, x u16, y u16                |
//! | 0x20 | Ping   | either       | token u64                                             |
//! | 0x21 | Pong   | either       | token u64                                             |
//! | 0x30 | RequestKeyframe | client->host | (empty)                                      |
//!
//! Touch coordinates are normalised: `0` is the left/top edge of the video and
//! `65535` the right/bottom edge, so the client never needs to know the stream
//! size in pixels.

use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_PORT: u16 = 7420;

pub const HEADER_LEN: usize = 5;
/// Hard cap on a single payload. A 4K keyframe fits comfortably; anything
/// bigger is a corrupt or hostile stream.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

pub const KIND_HELLO: u8 = 0x01;
pub const KIND_CONFIG: u8 = 0x02;
pub const KIND_VIDEO: u8 = 0x03;
pub const KIND_TOUCH: u8 = 0x10;
pub const KIND_PING: u8 = 0x20;
pub const KIND_PONG: u8 = 0x21;
pub const KIND_REQUEST_KEYFRAME: u8 = 0x30;

pub const CODEC_H264: u8 = 1;

/// Video flag: this access unit starts with an IDR frame.
pub const FLAG_KEYFRAME: u8 = 0b01;
/// Video flag: this access unit carries SPS/PPS (codec config) in-band.
pub const FLAG_CONFIG: u8 = 0b10;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("unknown message kind 0x{0:02x}")]
    UnknownKind(u8),
    #[error("payload too short for kind 0x{kind:02x}: need {need} bytes, got {got}")]
    Truncated { kind: u8, need: usize, got: usize },
    #[error("payload of {0} bytes exceeds the {MAX_PAYLOAD} byte limit")]
    TooLarge(usize),
    #[error("unknown touch action {0}")]
    BadTouchAction(u8),
    #[error("hello name is not valid utf-8")]
    BadName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchAction {
    Down = 0,
    Move = 1,
    Up = 2,
    Cancel = 3,
}

impl TouchAction {
    fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            0 => Ok(Self::Down),
            1 => Ok(Self::Move),
            2 => Ok(Self::Up),
            3 => Ok(Self::Cancel),
            other => Err(ProtoError::BadTouchAction(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello {
        version: u16,
        width: u32,
        height: u32,
        name: String,
    },
    Config {
        version: u16,
        width: u32,
        height: u32,
        codec: u8,
    },
    Video {
        pts_us: u64,
        flags: u8,
        data: Vec<u8>,
    },
    Touch {
        action: TouchAction,
        pointer_id: u8,
        x: u16,
        y: u16,
    },
    Ping(u64),
    Pong(u64),
    /// Client's decoder (re)started and needs an IDR to begin from.
    RequestKeyframe,
}

/// Split a 5-byte header into `(kind, payload_len)`.
pub fn parse_header(header: [u8; HEADER_LEN]) -> Result<(u8, usize), ProtoError> {
    let kind = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(ProtoError::TooLarge(len));
    }
    Ok((kind, len))
}

fn need(kind: u8, payload: &[u8], n: usize) -> Result<(), ProtoError> {
    if payload.len() < n {
        Err(ProtoError::Truncated {
            kind,
            need: n,
            got: payload.len(),
        })
    } else {
        Ok(())
    }
}

impl Message {
    pub fn kind(&self) -> u8 {
        match self {
            Message::Hello { .. } => KIND_HELLO,
            Message::Config { .. } => KIND_CONFIG,
            Message::Video { .. } => KIND_VIDEO,
            Message::Touch { .. } => KIND_TOUCH,
            Message::Ping(_) => KIND_PING,
            Message::Pong(_) => KIND_PONG,
            Message::RequestKeyframe => KIND_REQUEST_KEYFRAME,
        }
    }

    /// Serialise to a complete frame (header + payload), ready to write.
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        match self {
            Message::Hello {
                version,
                width,
                height,
                name,
            } => {
                payload.extend_from_slice(&version.to_be_bytes());
                payload.extend_from_slice(&width.to_be_bytes());
                payload.extend_from_slice(&height.to_be_bytes());
                payload.extend_from_slice(name.as_bytes());
            }
            Message::Config {
                version,
                width,
                height,
                codec,
            } => {
                payload.extend_from_slice(&version.to_be_bytes());
                payload.extend_from_slice(&width.to_be_bytes());
                payload.extend_from_slice(&height.to_be_bytes());
                payload.push(*codec);
            }
            Message::Video {
                pts_us,
                flags,
                data,
            } => {
                payload.reserve(9 + data.len());
                payload.extend_from_slice(&pts_us.to_be_bytes());
                payload.push(*flags);
                payload.extend_from_slice(data);
            }
            Message::Touch {
                action,
                pointer_id,
                x,
                y,
            } => {
                payload.push(*action as u8);
                payload.push(*pointer_id);
                payload.extend_from_slice(&x.to_be_bytes());
                payload.extend_from_slice(&y.to_be_bytes());
            }
            Message::Ping(t) | Message::Pong(t) => payload.extend_from_slice(&t.to_be_bytes()),
            Message::RequestKeyframe => {}
        }
        let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
        frame.push(self.kind());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// Parse a payload whose header has already been consumed.
    pub fn decode(kind: u8, p: &[u8]) -> Result<Self, ProtoError> {
        if p.len() > MAX_PAYLOAD {
            return Err(ProtoError::TooLarge(p.len()));
        }
        match kind {
            KIND_HELLO => {
                need(kind, p, 10)?;
                Ok(Message::Hello {
                    version: u16::from_be_bytes([p[0], p[1]]),
                    width: u32::from_be_bytes([p[2], p[3], p[4], p[5]]),
                    height: u32::from_be_bytes([p[6], p[7], p[8], p[9]]),
                    name: String::from_utf8(p[10..].to_vec()).map_err(|_| ProtoError::BadName)?,
                })
            }
            KIND_CONFIG => {
                need(kind, p, 11)?;
                Ok(Message::Config {
                    version: u16::from_be_bytes([p[0], p[1]]),
                    width: u32::from_be_bytes([p[2], p[3], p[4], p[5]]),
                    height: u32::from_be_bytes([p[6], p[7], p[8], p[9]]),
                    codec: p[10],
                })
            }
            KIND_VIDEO => {
                need(kind, p, 9)?;
                let mut pts = [0u8; 8];
                pts.copy_from_slice(&p[..8]);
                Ok(Message::Video {
                    pts_us: u64::from_be_bytes(pts),
                    flags: p[8],
                    data: p[9..].to_vec(),
                })
            }
            KIND_TOUCH => {
                need(kind, p, 6)?;
                Ok(Message::Touch {
                    action: TouchAction::from_u8(p[0])?,
                    pointer_id: p[1],
                    x: u16::from_be_bytes([p[2], p[3]]),
                    y: u16::from_be_bytes([p[4], p[5]]),
                })
            }
            KIND_PING | KIND_PONG => {
                need(kind, p, 8)?;
                let mut t = [0u8; 8];
                t.copy_from_slice(&p[..8]);
                let t = u64::from_be_bytes(t);
                Ok(if kind == KIND_PING {
                    Message::Ping(t)
                } else {
                    Message::Pong(t)
                })
            }
            KIND_REQUEST_KEYFRAME => Ok(Message::RequestKeyframe),
            other => Err(ProtoError::UnknownKind(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let frame = msg.encode();
        let (kind, len) = parse_header(frame[..HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - HEADER_LEN);
        assert_eq!(Message::decode(kind, &frame[HEADER_LEN..]).unwrap(), msg);
    }

    #[test]
    fn roundtrips() {
        roundtrip(Message::Hello {
            version: PROTOCOL_VERSION,
            width: 2400,
            height: 1080,
            name: "Pixel 7".into(),
        });
        roundtrip(Message::Config {
            version: PROTOCOL_VERSION,
            width: 1920,
            height: 1080,
            codec: CODEC_H264,
        });
        roundtrip(Message::Video {
            pts_us: 123_456_789,
            flags: FLAG_KEYFRAME | FLAG_CONFIG,
            data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
        });
        roundtrip(Message::Touch {
            action: TouchAction::Move,
            pointer_id: 3,
            x: 65535,
            y: 0,
        });
        roundtrip(Message::Ping(42));
        roundtrip(Message::Pong(u64::MAX));
        roundtrip(Message::RequestKeyframe);
    }

    #[test]
    fn empty_hello_name_is_ok() {
        roundtrip(Message::Hello {
            version: 1,
            width: 1,
            height: 1,
            name: String::new(),
        });
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(
            Message::decode(0x7f, &[]),
            Err(ProtoError::UnknownKind(0x7f))
        );
        assert!(matches!(
            Message::decode(KIND_TOUCH, &[0, 0, 0]),
            Err(ProtoError::Truncated { .. })
        ));
        assert_eq!(
            Message::decode(KIND_TOUCH, &[9, 0, 0, 0, 0, 0]),
            Err(ProtoError::BadTouchAction(9))
        );
        assert!(matches!(
            parse_header([KIND_VIDEO, 0xff, 0xff, 0xff, 0xff]),
            Err(ProtoError::TooLarge(_))
        ));
    }

    #[test]
    fn header_layout_is_stable() {
        // The Android client hardcodes this; don't change it by accident.
        assert_eq!(
            Message::Ping(1).encode(),
            vec![0x20, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(Message::RequestKeyframe.encode(), vec![0x30, 0, 0, 0, 0]);
    }
}
