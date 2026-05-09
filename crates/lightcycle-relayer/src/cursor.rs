//! Stream-resumption cursor.
//!
//! Wire format: `[height: 8 bytes BE][block_id: 32 bytes]`. 40 bytes
//! total, opaque to consumers. The firehose layer encodes/decodes
//! these on the gRPC envelope; the relayer just produces them.
//!
//! Including both the height AND the block_id is deliberate. The
//! height alone would let a client resume in a way that silently
//! crossed a reorg boundary; including the block_id makes resumption
//! reorg-aware — if the client's last-seen block_id no longer
//! matches our canonical chain at that height, we know the client
//! needs an UNDO walk before NEW deliveries can resume.

use lightcycle_types::{BlockHeight, BlockId};

/// Opaque resumption cursor encoding `(height, block_id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub height: BlockHeight,
    pub block_id: BlockId,
}

impl Cursor {
    pub fn new(height: BlockHeight, block_id: BlockId) -> Self {
        Self { height, block_id }
    }

    /// Encode as a 40-byte cursor blob: 8-byte BE height || 32-byte
    /// block_id. The exact bytes the firehose layer puts on the wire.
    pub fn to_bytes(&self) -> [u8; 40] {
        let mut out = [0u8; 40];
        out[..8].copy_from_slice(&self.height.to_be_bytes());
        out[8..].copy_from_slice(&self.block_id.0);
        out
    }

    /// Round-trip parse from a cursor blob. Returns None on
    /// wrong-length input — consumers should treat that as a
    /// malformed-cursor error and surface accordingly.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 40 {
            return None;
        }
        let mut height_bytes = [0u8; 8];
        height_bytes.copy_from_slice(&bytes[..8]);
        let height = BlockHeight::from_be_bytes(height_bytes);
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[8..]);
        Some(Self {
            height,
            block_id: BlockId(id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip() {
        let c = Cursor::new(82_531_247, BlockId([0xab; 32]));
        let bytes = c.to_bytes();
        let parsed = Cursor::from_bytes(&bytes).expect("parse");
        assert_eq!(parsed, c);
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        assert!(Cursor::from_bytes(&[0u8; 39]).is_none());
        assert!(Cursor::from_bytes(&[0u8; 41]).is_none());
        assert!(Cursor::from_bytes(&[]).is_none());
    }

    #[test]
    fn cursor_height_is_be_encoded() {
        // Sanity: a high enough height that all 8 bytes matter.
        let c = Cursor::new(0x0123_4567_89AB_CDEF, BlockId([0; 32]));
        let bytes = c.to_bytes();
        assert_eq!(&bytes[..8], &[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]);
    }
}
