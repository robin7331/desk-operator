//! The binary frames of a stream on the daemon's socket (`desk-stream-1`), used by the daemon
//! and by `desk stream`.
//!
//! Each frame is a 32-byte little-endian header and `length` payload bytes:
//!
//! | bytes | field      | type | meaning                                                    |
//! |-------|------------|------|------------------------------------------------------------|
//! | 0..4  | `magic`    | u32  | `0x4B534544`, the bytes `D E S K`                          |
//! | 4     | `kind`     | u8   | 0 data, 1 end                                              |
//! | 5     | `flags`    | u8   | 0                                                          |
//! | 6..8  | `reserved` | u16  | 0                                                          |
//! | 8..12 | `length`   | u32  | payload bytes after the header                             |
//! | 12..20| `seq`      | u64  | the block's number, from 0                                 |
//! | 20..28| `offset`   | u64  | its first unit since the start (samples for `logic`)       |
//! | 28..32| `dropped`  | u32  | blocks this subscriber lost so far                         |
//!
//! In an end frame `seq` and `offset` are the next ones, and the payload is a JSON envelope.

/// The framing's name in the header line.
pub const FRAMING: &str = "desk-stream-1";
/// "DESK" as little-endian bytes.
pub const MAGIC: u32 = 0x4B53_4544;
pub const HEADER_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Data,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub length: u32,
    pub seq: u64,
    pub offset: u64,
    pub dropped: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        h[4] = match self.kind {
            Kind::Data => 0,
            Kind::End => 1,
        };
        h[8..12].copy_from_slice(&self.length.to_le_bytes());
        h[12..20].copy_from_slice(&self.seq.to_le_bytes());
        h[20..28].copy_from_slice(&self.offset.to_le_bytes());
        h[28..32].copy_from_slice(&self.dropped.to_le_bytes());
        h
    }

    pub fn decode(h: &[u8; HEADER_LEN]) -> std::result::Result<Header, String> {
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        let u64_at = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&h[i..i + 8]);
            u64::from_le_bytes(b)
        };
        if u32_at(0) != MAGIC {
            return Err(format!(
                "not a desk stream frame (magic {:08x}, expected {MAGIC:08x})",
                u32_at(0)
            ));
        }
        let kind = match h[4] {
            0 => Kind::Data,
            1 => Kind::End,
            k => return Err(format!("unknown frame kind {k}")),
        };
        Ok(Header {
            kind,
            length: u32_at(8),
            seq: u64_at(12),
            offset: u64_at(20),
            dropped: u32_at(28),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_layout() {
        let h = Header {
            kind: Kind::Data,
            length: 1 << 20,
            seq: 7,
            offset: 0x0102_0304_0506_0708,
            dropped: 3,
        };
        let b = h.encode();
        assert_eq!(&b[0..4], b"DESK");
        assert_eq!(b[4], 0);
        assert_eq!(&b[5..8], &[0, 0, 0]);
        assert_eq!(&b[8..12], &[0, 0, 0x10, 0]);
        assert_eq!(b[12], 7);
        assert_eq!(b[20], 8, "little-endian");
        assert_eq!(b[28], 3);
        assert_eq!(Header::decode(&b).unwrap(), h);
        let end = Header {
            kind: Kind::End,
            ..h
        };
        assert_eq!(end.encode()[4], 1);
        assert_eq!(Header::decode(&end.encode()).unwrap(), end);
    }

    #[test]
    fn refuses_what_isnt_a_frame() {
        let mut b = [0u8; HEADER_LEN];
        assert!(Header::decode(&b).unwrap_err().contains("magic"));
        b[0..4].copy_from_slice(b"DESK");
        b[4] = 9;
        assert!(Header::decode(&b).unwrap_err().contains("kind"));
    }
}
