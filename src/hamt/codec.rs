use alloc::vec::Vec;

use super::hash::StructuralHash;

/// A representation of an internal node that is safe to persist to disk.
///
/// It contains the layout necessary to reconstruct the node's structure and
/// lazy-load its children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedInternalNode {
    pub datamap: u32,
    pub nodemap: u32,
    pub structural_hash: StructuralHash,
    pub child_hashes: Vec<StructuralHash>,
}

impl PersistedInternalNode {
    /// Encodes the node to a dense binary format.
    ///
    /// Layout:
    /// - Version (1 byte): `0x01`
    /// - Datamap (4 bytes, LE)
    /// - Nodemap (4 bytes, LE)
    /// - Structural hash (16 bytes)
    /// - Child count (4 bytes, LE)
    /// - Child hashes (`16 * child_count` bytes)
    ///
    /// # Panics
    /// Panics if the node has too many child hashes to fit in `u32`.
    #[must_use]
    pub fn encode_v1(&self) -> Vec<u8> {
        let child_count =
            u32::try_from(self.child_hashes.len()).expect("too many child hashes for v1 encoding");
        let child_bytes = self
            .child_hashes
            .len()
            .checked_mul(16)
            .expect("child hash payload size overflows usize");
        let capacity = 1_usize
            .checked_add(4)
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(16))
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(child_bytes))
            .expect("encoded node size overflows usize");
        let mut buf = Vec::with_capacity(capacity);
        buf.push(0x01);
        buf.extend_from_slice(&self.datamap.to_le_bytes());
        buf.extend_from_slice(&self.nodemap.to_le_bytes());
        buf.extend_from_slice(&self.structural_hash);
        buf.extend_from_slice(&child_count.to_le_bytes());
        for hash in &self.child_hashes {
            buf.extend_from_slice(hash);
        }
        buf
    }

    /// Decodes the node from a dense binary format.
    ///
    /// # Errors
    /// Returns an error when the version byte is invalid or the buffer is too
    /// short for the declared payload.
    pub fn decode_v1(buf: &[u8]) -> Result<Self, &'static str> {
        if buf.is_empty() || buf[0] != 0x01 {
            return Err("Invalid version byte");
        }
        if buf.len() < 29 {
            return Err("Buffer too short for v1 header");
        }

        let datamap = u32::from_le_bytes(
            buf.get(1..5)
                .ok_or("Buffer too short for datamap")?
                .try_into()
                .map_err(|_| "Buffer too short for datamap")?,
        );
        let nodemap = u32::from_le_bytes(
            buf.get(5..9)
                .ok_or("Buffer too short for nodemap")?
                .try_into()
                .map_err(|_| "Buffer too short for nodemap")?,
        );

        let mut structural_hash = [0u8; 16];
        structural_hash.copy_from_slice(&buf[9..25]);

        let child_count = u32::from_le_bytes(
            buf.get(25..29)
                .ok_or("Buffer too short for child count")?
                .try_into()
                .map_err(|_| "Buffer too short for child count")?,
        ) as usize;

        let child_bytes = child_count
            .checked_mul(16)
            .ok_or("Child hash payload size overflows usize")?;
        let total_len = 29_usize
            .checked_add(child_bytes)
            .ok_or("Child hash payload size overflows usize")?;
        if buf.len() < total_len {
            return Err("Buffer too short for child hashes");
        }

        let mut child_hashes = Vec::with_capacity(child_count);
        for i in 0..child_count {
            let start = 29_usize
                .checked_add(
                    i.checked_mul(16)
                        .ok_or("Child hash index overflows usize")?,
                )
                .ok_or("Child hash index overflows usize")?;
            let end = start
                .checked_add(16)
                .ok_or("Child hash index overflows usize")?;
            let mut hash = [0u8; 16];
            hash.copy_from_slice(&buf[start..end]);
            child_hashes.push(hash);
        }

        Ok(Self {
            datamap,
            nodemap,
            structural_hash,
            child_hashes,
        })
    }
}
