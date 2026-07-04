//! Compact chunk encoding for the wire (plan-game-systems §5): per-chunk
//! palette → bit-packed indices → LZ4. Typical terrain chunks land at 1–4 KB
//! instead of the 32 KB dense grid; uniform chunks (all air, solid stone)
//! collapse to 2 bytes, subsuming the old `empty` flag.
//!
//! Payload layout (first byte is the tag):
//! - `0` **Uniform**: `[0, id]` — every voxel is `id`.
//! - `1` **Paletted**: `[1, n] ++ palette[n] ++ lz4(bit-packed indices)` with
//!   `2 <= n <= 128` (1–7 bits per index, little-endian bit order).
//! - `2` **RawDense**: `[2] ++ lz4(raw 32768 bytes)` — fallback for
//!   pathological palettes (> 128 distinct ids, where packing wouldn't help).
//!
//! [`decode_chunk`] is the network attack surface: it must return `None` on
//! arbitrary/malformed input, never panic and never over-allocate.

use crate::coords::CHUNK_CUBED;
use crate::voxel::{ChunkVolume, Voxel};

/// Palettes above this size fall back to [`RawDense`]: at > 7 bits per index
/// the packing gains nothing over LZ4 on the raw grid.
const MAX_PALETTE: usize = 128;

const TAG_UNIFORM: u8 = 0;
const TAG_PALETTED: u8 = 1;
const TAG_RAW: u8 = 2;

/// Encode a chunk for the wire.
pub fn encode_chunk(v: &ChunkVolume) -> Vec<u8> {
    let raw = v.as_bytes();

    // Build the palette (order of first appearance, so encoding is
    // deterministic and golden-bytes tests stay stable).
    let mut index_of = [u8::MAX; 256];
    let mut palette: Vec<Voxel> = Vec::new();
    for &b in raw {
        if index_of[b as usize] == u8::MAX {
            if palette.len() >= 256 {
                break;
            }
            index_of[b as usize] = palette.len() as u8;
            palette.push(b);
        }
    }

    match palette.len() {
        1 => vec![TAG_UNIFORM, palette[0]],
        2..=MAX_PALETTE => {
            let bits = bits_for(palette.len());
            let mut packed = Vec::with_capacity(CHUNK_CUBED * bits as usize / 8 + 8);
            let mut acc: u64 = 0;
            let mut acc_bits: u32 = 0;
            for &b in raw {
                acc |= (index_of[b as usize] as u64) << acc_bits;
                acc_bits += bits;
                while acc_bits >= 8 {
                    packed.push(acc as u8);
                    acc >>= 8;
                    acc_bits -= 8;
                }
            }
            if acc_bits > 0 {
                packed.push(acc as u8);
            }
            let mut out = Vec::with_capacity(2 + palette.len() + packed.len() / 4);
            out.push(TAG_PALETTED);
            out.push(palette.len() as u8);
            out.extend_from_slice(&palette);
            out.extend_from_slice(&lz4_flex::compress_prepend_size(&packed));
            out
        }
        _ => {
            let mut out = Vec::with_capacity(1 + CHUNK_CUBED / 2);
            out.push(TAG_RAW);
            out.extend_from_slice(&lz4_flex::compress_prepend_size(raw));
            out
        }
    }
}

/// Decode a wire payload. `None` on any malformed input — this must hold for
/// *arbitrary* bytes (it's the attack surface), so every length, palette
/// index, and LZ4 size prefix is checked before use.
pub fn decode_chunk(bytes: &[u8]) -> Option<ChunkVolume> {
    match *bytes.first()? {
        TAG_UNIFORM => {
            let &[_, id] = bytes else { return None };
            let mut v = ChunkVolume::empty();
            if id != 0 {
                v.as_bytes_mut().fill(id);
            }
            Some(v)
        }
        TAG_PALETTED => {
            let n = *bytes.get(1)? as usize;
            if !(2..=MAX_PALETTE).contains(&n) {
                return None;
            }
            let palette = bytes.get(2..2 + n)?;
            let bits = bits_for(n);
            let packed = decompress(bytes.get(2 + n..)?, CHUNK_CUBED * bits as usize / 8 + 1)?;
            let mask = (1u64 << bits) - 1;
            let mut v = ChunkVolume::empty();
            let out = v.as_bytes_mut();
            let mut acc: u64 = 0;
            let mut acc_bits: u32 = 0;
            let mut src = packed.iter();
            for slot in out.iter_mut() {
                while acc_bits < bits {
                    acc |= (*src.next()? as u64) << acc_bits;
                    acc_bits += 8;
                }
                let idx = (acc & mask) as usize;
                acc >>= bits;
                acc_bits -= bits;
                *slot = *palette.get(idx)?;
            }
            Some(v)
        }
        TAG_RAW => {
            let raw = decompress(bytes.get(1..)?, CHUNK_CUBED)?;
            (raw.len() == CHUNK_CUBED).then(|| ChunkVolume::from_bytes(&raw))
        }
        _ => None,
    }
}

/// True if a payload encodes an all-air chunk, without decoding it.
pub fn payload_is_air(bytes: &[u8]) -> bool {
    bytes == [TAG_UNIFORM, 0]
}

fn bits_for(n: usize) -> u32 {
    usize::BITS - (n - 1).leading_zeros()
}

/// Size-checked LZ4 block decompression: rejects size prefixes beyond
/// `max_len` before allocating, so a hostile payload can't demand memory.
fn decompress(data: &[u8], max_len: usize) -> Option<Vec<u8>> {
    let prefix = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
    if prefix > max_len {
        return None;
    }
    lz4_flex::decompress_size_prepended(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::CHUNK_CUBED;

    /// Deterministic pseudo-random voxels without a rand dependency.
    fn lcg_volume(seed: u64, ids: &[u8]) -> ChunkVolume {
        let mut v = ChunkVolume::empty();
        let mut s = seed;
        let out = v.as_bytes_mut();
        for slot in out.iter_mut() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *slot = ids[(s >> 33) as usize % ids.len()];
        }
        v
    }

    #[test]
    fn golden_bytes_uniform() {
        assert_eq!(encode_chunk(&ChunkVolume::empty()), vec![0, 0]);
        let mut solid = ChunkVolume::empty();
        solid.as_bytes_mut().fill(3);
        assert_eq!(encode_chunk(&solid), vec![0, 3]);
        assert!(payload_is_air(&encode_chunk(&ChunkVolume::empty())));
        assert!(!payload_is_air(&encode_chunk(&solid)));
    }

    #[test]
    fn golden_bytes_two_id_header() {
        // Half air, half stone: tag 1, palette [0, 1], 1 bit per index.
        let mut v = ChunkVolume::empty();
        for i in 0..CHUNK_CUBED / 2 {
            v.as_bytes_mut()[i] = 1;
        }
        let enc = encode_chunk(&v);
        assert_eq!(&enc[..4], &[1, 2, 1, 0], "tag, n, palette in first-seen order");
        assert!(enc.len() < 200, "1-bit half/half packs tiny under LZ4, got {}", enc.len());
    }

    #[test]
    fn round_trips_across_palette_sizes() {
        let cases: Vec<ChunkVolume> = vec![
            ChunkVolume::empty(),
            lcg_volume(1, &[0, 5]),
            lcg_volume(2, &[0, 1, 2, 3, 4, 5, 6, 7]),                 // 3 bits
            lcg_volume(3, &(0..=100).collect::<Vec<u8>>()),           // 7 bits
            lcg_volume(4, &(0..=200).collect::<Vec<u8>>()),           // RawDense
        ];
        for (i, v) in cases.iter().enumerate() {
            let enc = encode_chunk(v);
            let dec = decode_chunk(&enc).unwrap_or_else(|| panic!("case {i} failed to decode"));
            assert_eq!(dec.as_bytes(), v.as_bytes(), "case {i} round-trip mismatch");
        }
    }

    #[test]
    fn terrain_like_chunk_compresses_hard() {
        // Layered terrain (air over soil over stone) with a sprinkle of ore:
        // the shape a real surface chunk has. Must land far under the old
        // 32 KB dense encoding.
        let mut v = ChunkVolume::empty();
        for y in 0..32 {
            for x in 0..32 {
                for z in 0..32 {
                    let id = match y {
                        0..12 => 3,
                        12..16 => 2,
                        16 => 1,
                        _ => 0,
                    };
                    v.set(x, y as i32, z, id);
                }
            }
        }
        v.set(5, 5, 5, 7);
        let enc = encode_chunk(&v);
        assert!(enc.len() < 2048, "terrain chunk should encode ≤ 2 KB, got {}", enc.len());
        assert_eq!(decode_chunk(&enc).unwrap().as_bytes(), v.as_bytes());
    }

    #[test]
    fn decode_never_panics_on_malformed_input() {
        // Truncations and bit-flips of every valid encoding, plus junk.
        let samples =
            [encode_chunk(&ChunkVolume::empty()), encode_chunk(&lcg_volume(7, &[0, 1, 2, 9]))];
        for enc in &samples {
            for cut in 0..enc.len().min(64) {
                let _ = decode_chunk(&enc[..cut]);
            }
            let mut s = 0xdeadbeefu64;
            for _ in 0..2000 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let mut m = enc.clone();
                let i = (s >> 33) as usize % m.len();
                m[i] ^= (s >> 17) as u8 | 1;
                let _ = decode_chunk(&m); // must not panic; None or a volume both fine
            }
        }
        assert!(decode_chunk(&[]).is_none());
        assert!(decode_chunk(&[9, 9, 9]).is_none());
        // A size prefix demanding gigabytes must be rejected, not allocated.
        let mut huge = vec![TAG_RAW];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend_from_slice(&[0; 16]);
        assert!(decode_chunk(&huge).is_none());
    }
}
