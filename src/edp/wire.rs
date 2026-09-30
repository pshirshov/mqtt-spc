//! EDP v2 framing, checksum and AES-128-ECB encryption.
//!
//! Wire layout (little-endian), offsets including the 2-byte length prefix:
//!
//! | Offset | Bytes | Field                                        |
//! |--------|-------|----------------------------------------------|
//! | 0      | 2     | Remaining frame length                       |
//! | 2      | 1     | Protocol ID `0x45`                           |
//! | 3      | 1     | Version `0x02`                               |
//! | 4      | 1     | Flags: bit 0 encrypted, bit 3 from receiver  |
//! | 5      | 4     | Sequence                                     |
//! | 9      | 4     | Source ID                                    |
//! | 13     | 4     | Destination ID                               |
//! | 17     | 1     | Major code                                   |
//! | 18     | 1     | Minor code                                   |
//! | 19     | 2     | Checksum                                     |
//! | 21     | 2     | Payload length                               |
//! | 23     | n     | Payload                                      |
//!
//! Protocol reverse-engineered by <https://github.com/imduffy15/spcedp>.

use std::fmt;

use aes::Aes128;
use aes::cipher::{Array, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use tracing::warn;

const PROTOCOL_BYTE: u8 = 0x45;
const PROTOCOL_VERSION: u8 = 0x02;
const PREFIX_LEN: usize = 2;
/// Wire header length including the length prefix.
const HEADER_LEN: usize = 23;
/// Struct (wire minus prefix) header length.
const STRUCT_HEADER_LEN: usize = HEADER_LEN - PREFIX_LEN;
const STRUCT_CHECKSUM_OFFSET: usize = 17;
/// Struct offset where the encrypted region starts (major code onwards).
const STRUCT_ENCRYPT_OFFSET: usize = 15;
const AES_BLOCK: usize = 16;

pub const FLAG_ENCRYPTED: u8 = 0x01;
pub const FLAG_FROM_RECEIVER: u8 = 0x08;

const CHECKSUM_INIT: u16 = 0xFFFF;
const CHECKSUM_POLY: u16 = 0xA097;

/// Real frames are small (XML replies fragment at ~1.4 KB); a valid-looking
/// header claiming more than this is treated as garbage and resynced past.
const MAX_PLAUSIBLE_FRAME: usize = 16384;
/// Bytes the decoder may discard while hunting for a frame boundary before
/// declaring the stream corrupt (or the key wrong).
const RESYNC_LIMIT: usize = 8192;

pub mod major {
    pub const SESSION: u8 = 1;
    pub const EVENT: u8 = 2;
    pub const BINARY_CMD: u8 = 4;
    pub const XML_CMD: u8 = 10;
}

pub mod minor {
    pub const POLL: u8 = 0;
    pub const POLL_ACK: u8 = 1;
    pub const HELLO: u8 = 2;
    pub const HELLO_ACK: u8 = 3;
    pub const EVENT_PUSH: u8 = 0;
    pub const EVENT_ACK: u8 = 1;
    pub const REQUEST: u8 = 0;
    pub const XML_REPLY: u8 = 1;
    pub const BINARY_REPLY: u8 = 2;
}

/// 16-byte EDP AES key.
#[derive(Clone)]
pub struct EdpKey(Aes128);

impl EdpKey {
    /// Parse the panel's 32-hex-digit key representation.
    pub fn from_hex(hex: &str) -> Result<Self, String> {
        let hex = hex.trim();
        if hex.len() != 2 * AES_BLOCK || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("EDP key must be exactly 32 hex digits".into());
        }
        let mut key = [0u8; AES_BLOCK];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                .map_err(|e| format!("invalid EDP key: {e}"))?;
        }
        Ok(Self(Aes128::new(&Array::from(key))))
    }
}

impl fmt::Debug for EdpKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EdpKey(<redacted>)")
    }
}

#[derive(Debug)]
pub enum DecodeError {
    /// The frame is encrypted but no key is configured; every later frame
    /// will be too, so resyncing is pointless.
    EncryptionRequired,
    /// Could not find a frame boundary within the resync budget.
    Corrupt(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncryptionRequired => {
                f.write_str("panel sends encrypted frames but no EDP key is configured")
            }
            Self::Corrupt(msg) => write!(f, "corrupt EDP stream: {msg}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub flags: u8,
    pub sequence: u32,
    pub src_id: u32,
    pub dst_id: u32,
    pub major: u8,
    pub minor: u8,
    pub payload: Vec<u8>,
}

/// EDP v2 frame checksum over struct bytes (wire minus length prefix),
/// header plus payload, skipping the checksum slot.
///
/// A 16-bit shift register where each byte is *added* into the low half
/// without carrying into the high half, XORed with the polynomial when the
/// shifted-out bit was set.
fn checksum(struct_bytes: &[u8]) -> u16 {
    let mut state = CHECKSUM_INIT;
    for (i, &byte) in struct_bytes.iter().enumerate() {
        if i == STRUCT_CHECKSUM_OFFSET || i == STRUCT_CHECKSUM_OFFSET + 1 {
            continue;
        }
        let carry = state & 0x8000 != 0;
        let shifted = state << 1;
        let low = (shifted as u8).wrapping_add(byte);
        state = (shifted & 0xFF00) | u16::from(low);
        if carry {
            state ^= CHECKSUM_POLY;
        }
    }
    state
}

impl Frame {
    pub fn encode(&self, key: Option<&EdpKey>) -> Vec<u8> {
        let payload_len =
            u16::try_from(self.payload.len()).expect("EDP payload exceeds 16-bit length field");
        let flags = match key {
            Some(_) => self.flags | FLAG_ENCRYPTED,
            None => self.flags & !FLAG_ENCRYPTED,
        };

        let mut body = Vec::with_capacity(STRUCT_HEADER_LEN + self.payload.len() + AES_BLOCK);
        body.push(PROTOCOL_BYTE);
        body.push(PROTOCOL_VERSION);
        body.push(flags);
        body.extend_from_slice(&self.sequence.to_le_bytes());
        body.extend_from_slice(&self.src_id.to_le_bytes());
        body.extend_from_slice(&self.dst_id.to_le_bytes());
        body.push(self.major);
        body.push(self.minor);
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&payload_len.to_le_bytes());
        body.extend_from_slice(&self.payload);

        let csum = checksum(&body);
        body[STRUCT_CHECKSUM_OFFSET..STRUCT_CHECKSUM_OFFSET + 2]
            .copy_from_slice(&csum.to_le_bytes());

        if let Some(key) = key {
            let tail_len = body.len() - STRUCT_ENCRYPT_OFFSET;
            body.resize(
                body.len() + (AES_BLOCK - tail_len % AES_BLOCK) % AES_BLOCK,
                0,
            );
            for block in body[STRUCT_ENCRYPT_OFFSET..].as_chunks_mut::<AES_BLOCK>().0 {
                key.0.encrypt_block(<&mut aes::Block>::from(block));
            }
        }

        let rem = u16::try_from(body.len()).expect("EDP frame exceeds 16-bit length prefix");
        let mut wire = Vec::with_capacity(PREFIX_LEN + body.len());
        wire.extend_from_slice(&rem.to_le_bytes());
        wire.extend_from_slice(&body);
        wire
    }

    /// Decode one complete wire frame (length prefix included, exact length).
    fn decode(wire: &[u8], key: Option<&EdpKey>) -> Result<Self, FrameError> {
        let mut body = wire[PREFIX_LEN..].to_vec();
        let flags = body[2];
        if flags & FLAG_ENCRYPTED != 0 {
            let Some(key) = key else {
                return Err(FrameError::EncryptionRequired);
            };
            let tail = &mut body[STRUCT_ENCRYPT_OFFSET..];
            if !tail.len().is_multiple_of(AES_BLOCK) {
                return Err(FrameError::Invalid(format!(
                    "encrypted tail length {} is not a multiple of {AES_BLOCK}",
                    tail.len()
                )));
            }
            for block in tail.as_chunks_mut::<AES_BLOCK>().0 {
                key.0.decrypt_block(<&mut aes::Block>::from(block));
            }
        }

        let u32_at = |o: usize| u32::from_le_bytes(body[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(body[o..o + 2].try_into().unwrap());
        let payload_len = usize::from(u16_at(19));
        let end = STRUCT_HEADER_LEN + payload_len;
        if end > body.len() {
            return Err(FrameError::Invalid(format!(
                "payload length {payload_len} overflows frame of {} bytes",
                body.len()
            )));
        }
        // The checksum is computed with the encrypted flag set, so verifying
        // it here also rejects a wrong key.
        if checksum(&body[..end]) != u16_at(STRUCT_CHECKSUM_OFFSET) {
            return Err(FrameError::Invalid("checksum mismatch".into()));
        }

        Ok(Self {
            flags: flags & !FLAG_ENCRYPTED,
            sequence: u32_at(3),
            src_id: u32_at(7),
            dst_id: u32_at(11),
            major: body[15],
            minor: body[16],
            payload: body[STRUCT_HEADER_LEN..end].to_vec(),
        })
    }
}

enum FrameError {
    EncryptionRequired,
    Invalid(String),
}

/// Splits a TCP byte stream into frames, resyncing byte-by-byte past garbage.
pub struct FrameDecoder {
    buf: Vec<u8>,
    key: Option<EdpKey>,
    /// Bytes dropped since the last good frame.
    dropped: usize,
    /// `(claimed_total, buf_len_when_wait_began)` for an incomplete frame.
    waiting: Option<(usize, usize)>,
}

impl FrameDecoder {
    pub fn new(key: Option<EdpKey>) -> Self {
        Self {
            buf: Vec::new(),
            key,
            dropped: 0,
            waiting: None,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Frame>, DecodeError> {
        self.buf.extend_from_slice(data);
        let mut frames = Vec::new();
        while self.buf.len() >= HEADER_LEN {
            let total = usize::from(u16::from_le_bytes([self.buf[0], self.buf[1]])) + PREFIX_LEN;
            let looks_valid = (HEADER_LEN..=MAX_PLAUSIBLE_FRAME).contains(&total)
                && self.buf[2] == PROTOCOL_BYTE
                && self.buf[3] == PROTOCOL_VERSION;
            if !looks_valid {
                self.waiting = None;
                self.resync()?;
                continue;
            }
            if self.buf.len() < total {
                // Bound the wait so a phantom length prefix cannot wedge the
                // decoder while real frames pile up behind it.
                let started = match self.waiting {
                    Some((claimed, started)) if claimed == total => started,
                    _ => {
                        self.waiting = Some((total, self.buf.len()));
                        self.buf.len()
                    }
                };
                if self.buf.len() - started > RESYNC_LIMIT {
                    warn!("EDP frame claiming {total} bytes never completed; resyncing");
                    self.waiting = None;
                    self.resync()?;
                    continue;
                }
                break;
            }
            self.waiting = None;
            match Frame::decode(&self.buf[..total], self.key.as_ref()) {
                Ok(frame) => {
                    self.buf.drain(..total);
                    self.dropped = 0;
                    frames.push(frame);
                }
                Err(FrameError::EncryptionRequired) => return Err(DecodeError::EncryptionRequired),
                Err(FrameError::Invalid(msg)) => {
                    warn!("Dropping undecodable EDP frame: {msg}");
                    self.resync()?;
                }
            }
        }
        Ok(frames)
    }

    fn resync(&mut self) -> Result<(), DecodeError> {
        self.buf.remove(0);
        self.dropped += 1;
        if self.dropped > RESYNC_LIMIT {
            return Err(DecodeError::Corrupt(format!(
                "dropped {} bytes without finding a valid frame (wrong EDP key?)",
                self.dropped
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Captured from a live SPC4300 (spcedp tests/test_checksum.py).
    const HELLO: &str = "1d 00 45 02 00 10 15 58 0f e8 03 00 00 e9 03 00 00 01 02 38 9d 08 00 \
                         24 cf 64 45 66 0c a4 3d";
    const HELLO_ACK: &str = "1d 00 45 02 08 10 15 58 0f e9 03 00 00 e8 03 00 00 01 03 8c cb 08 00 \
                             24 cf 64 45 66 0c a4 3d";
    const SIA_EVENT: &str = "45 00 45 02 00 12 15 58 0f e8 03 00 00 e9 03 00 00 02 00 74 7a 30 00 \
        45 32 5b 23 31 30 30 30 7c 30 39 31 35 33 34 30 33 30 36 32 30 32 36 \
        7c 4e 52 7c 30 7c 49 50 20 4c 69 6e 6b 20 52 65 73 74 6f 72 65 7c 7c 30 5d";

    fn key() -> EdpKey {
        EdpKey::from_hex("00112233445566778899AABBCCDDEEFF").unwrap()
    }

    #[test]
    fn captured_frames_roundtrip_bit_exact() {
        for capture in [HELLO, HELLO_ACK, SIA_EVENT] {
            let wire = hex(capture);
            let frame = Frame::decode(&wire, None).ok().expect("decode");
            assert_eq!(frame.encode(None), wire);
        }
    }

    #[test]
    fn captured_hello_fields() {
        let frame = Frame::decode(&hex(HELLO), None).ok().unwrap();
        assert_eq!(frame.src_id, 1000);
        assert_eq!(frame.dst_id, 1001);
        assert_eq!((frame.major, frame.minor), (major::SESSION, minor::HELLO));
        assert_eq!(frame.payload.len(), 8);
        let event = Frame::decode(&hex(SIA_EVENT), None).ok().unwrap();
        assert_eq!(
            event.payload,
            b"E2[#1000|09153403062026|NR|0|IP Link Restore||0]"
        );
    }

    #[test]
    fn corrupted_checksum_is_rejected() {
        let mut wire = hex(HELLO);
        *wire.last_mut().unwrap() ^= 0xFF;
        assert!(matches!(
            Frame::decode(&wire, None),
            Err(FrameError::Invalid(_))
        ));
    }

    #[test]
    fn encrypted_roundtrip_keeps_clear_header() {
        let frame = Frame::decode(&hex(HELLO), None).ok().unwrap();
        let clear = frame.encode(None);
        let enc = frame.encode(Some(&key()));
        assert_eq!(enc[4] & FLAG_ENCRYPTED, FLAG_ENCRYPTED);
        assert_eq!(&enc[5..17], &clear[5..17]);
        assert_eq!((enc.len() - 17) % AES_BLOCK, 0);
        let decoded = Frame::decode(&enc, Some(&key())).ok().unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn wrong_key_fails_checksum() {
        let frame = Frame::decode(&hex(HELLO), None).ok().unwrap();
        let enc = frame.encode(Some(&key()));
        let other = EdpKey::from_hex("ffeeddccbbaa99887766554433221100").unwrap();
        assert!(matches!(
            Frame::decode(&enc, Some(&other)),
            Err(FrameError::Invalid(_))
        ));
    }

    #[test]
    fn decoder_splits_and_resyncs() {
        let mut stream = vec![0xAA, 0x45, 0x02];
        stream.extend(hex(HELLO));
        stream.extend(hex(SIA_EVENT));
        let mut dec = FrameDecoder::new(None);
        let mut frames = Vec::new();
        for chunk in stream.chunks(5) {
            frames.extend(dec.feed(chunk).unwrap());
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].major, major::EVENT);
    }

    #[test]
    fn decoder_reports_missing_key() {
        let frame = Frame::decode(&hex(HELLO), None).ok().unwrap();
        let mut dec = FrameDecoder::new(None);
        assert!(matches!(
            dec.feed(&frame.encode(Some(&key()))),
            Err(DecodeError::EncryptionRequired)
        ));
    }

    #[test]
    fn key_parsing() {
        assert!(EdpKey::from_hex("0011").is_err());
        assert!(EdpKey::from_hex("zz112233445566778899AABBCCDDEEFF").is_err());
        assert!(EdpKey::from_hex(" 00112233445566778899aabbccddeeff\n").is_ok());
    }
}
