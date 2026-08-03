//! Packing of `(world_id, chunk x/y/z)` into a single `u64`.
//!
//! This is the key the SpacetimeDB module uses for its `chunk_blob` and
//! `chunk_edit` tables. It lives here, beside the wire codecs, because **both
//! the module and the server must agree on the layout byte-for-byte** — a
//! drift between the two would silently alias or lose chunks. Same rule as
//! the rest of this crate: one shared implementation, no second copy.
//!
//! SpacetimeDB allows one `#[primary_key]` column, so the world discriminator
//! and the chunk coordinate share a key. Layout, high bits first:
//!
//! | bits  | field    | range                    |
//! |-------|----------|--------------------------|
//! | 48–64 | world_id | `0 ..= 65535`            |
//! | 28–48 | cx       | `-524288 ..= 524287`     |
//! | 20–28 | cy       | `-128 ..= 127`           |
//! |  0–20 | cz       | `-524288 ..= 524287`     |
//!
//! At `CHUNK_SIZE = 32` that is ±16.7M voxels horizontally and ±4096
//! vertically — far beyond anything the generator produces. Out-of-range
//! coordinates are rejected rather than silently wrapped, because a wrapped
//! key would alias two distinct chunks onto one row and corrupt the world.

const WORLD_BITS: u32 = 16;
const CX_BITS: u32 = 20;
const CY_BITS: u32 = 8;
const CZ_BITS: u32 = 20;

const CX_SHIFT: u32 = CY_BITS + CZ_BITS;
const CY_SHIFT: u32 = CZ_BITS;
const WORLD_SHIFT: u32 = CX_BITS + CY_BITS + CZ_BITS;

const fn mask(bits: u32) -> u64 {
    (1u64 << bits) - 1
}

/// Inclusive bounds of a `bits`-wide two's-complement field.
const fn range(bits: u32) -> (i32, i32) {
    let half = 1i32 << (bits - 1);
    (-half, half - 1)
}

pub const CX_RANGE: (i32, i32) = range(CX_BITS);
pub const CY_RANGE: (i32, i32) = range(CY_BITS);
pub const CZ_RANGE: (i32, i32) = range(CZ_BITS);

const fn enc(v: i32, bits: u32) -> u64 {
    (v as i64 as u64) & mask(bits)
}

const fn dec(raw: u64, bits: u32) -> i32 {
    let sign = 1i64 << (bits - 1);
    (((raw & mask(bits)) as i64) ^ sign).wrapping_sub(sign) as i32
}

const fn in_range(v: i32, r: (i32, i32)) -> bool {
    v >= r.0 && v <= r.1
}

/// Whether `(cx, cy, cz)` is representable. Callers should check this before
/// [`pack_chunk_key`] and reject the request otherwise.
pub const fn coords_in_range(cx: i32, cy: i32, cz: i32) -> bool {
    in_range(cx, CX_RANGE) && in_range(cy, CY_RANGE) && in_range(cz, CZ_RANGE)
}

/// Pack a world + chunk coordinate. Returns `None` if any component is out of
/// range (see [`coords_in_range`]).
pub const fn pack_chunk_key(world_id: u16, cx: i32, cy: i32, cz: i32) -> Option<u64> {
    if !coords_in_range(cx, cy, cz) {
        return None;
    }
    Some(
        ((world_id as u64) << WORLD_SHIFT)
            | (enc(cx, CX_BITS) << CX_SHIFT)
            | (enc(cy, CY_BITS) << CY_SHIFT)
            | enc(cz, CZ_BITS),
    )
}

/// Inverse of [`pack_chunk_key`].
pub const fn unpack_chunk_key(key: u64) -> (u16, i32, i32, i32) {
    (
        ((key >> WORLD_SHIFT) & mask(WORLD_BITS)) as u16,
        dec(key >> CX_SHIFT, CX_BITS),
        dec(key >> CY_SHIFT, CY_BITS),
        dec(key, CZ_BITS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(world: u16, cx: i32, cy: i32, cz: i32) {
        let key = pack_chunk_key(world, cx, cy, cz).expect("in range");
        assert_eq!(unpack_chunk_key(key), (world, cx, cy, cz), "round trip {world} {cx} {cy} {cz}");
    }

    #[test]
    fn round_trips_origin_and_positives() {
        round_trip(0, 0, 0, 0);
        round_trip(1, 1, 1, 1);
        round_trip(7, 1234, 12, 5678);
    }

    #[test]
    fn round_trips_negatives() {
        round_trip(0, -1, -1, -1);
        round_trip(3, -1234, -12, -5678);
        // Mixed signs are the case a naive shift-and-mask gets wrong.
        round_trip(2, -1, 5, -9999);
        round_trip(2, 9999, -5, 1);
    }

    #[test]
    fn round_trips_extremes() {
        round_trip(u16::MAX, CX_RANGE.0, CY_RANGE.0, CZ_RANGE.0);
        round_trip(u16::MAX, CX_RANGE.1, CY_RANGE.1, CZ_RANGE.1);
        round_trip(0, CX_RANGE.0, CY_RANGE.1, CZ_RANGE.0);
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(pack_chunk_key(0, CX_RANGE.1 + 1, 0, 0).is_none());
        assert!(pack_chunk_key(0, CX_RANGE.0 - 1, 0, 0).is_none());
        assert!(pack_chunk_key(0, 0, CY_RANGE.1 + 1, 0).is_none());
        assert!(pack_chunk_key(0, 0, CY_RANGE.0 - 1, 0).is_none());
        assert!(pack_chunk_key(0, 0, 0, CZ_RANGE.1 + 1).is_none());
        assert!(pack_chunk_key(0, 0, 0, CZ_RANGE.0 - 1).is_none());
    }

    /// Distinct chunks must never alias onto one key — that would silently
    /// merge two chunks' storage.
    #[test]
    fn distinct_coords_give_distinct_keys() {
        let mut seen = std::collections::HashSet::new();
        for world in [0u16, 1, 65535] {
            for cx in [-3i32, -1, 0, 1, 3] {
                for cy in [-2i32, 0, 2] {
                    for cz in [-3i32, -1, 0, 1, 3] {
                        let key = pack_chunk_key(world, cx, cy, cz).expect("in range");
                        assert!(seen.insert(key), "collision at {world} {cx} {cy} {cz}");
                    }
                }
            }
        }
    }

    /// The world discriminator must actually separate worlds.
    #[test]
    fn world_id_separates_keys() {
        assert_ne!(pack_chunk_key(0, 5, 5, 5), pack_chunk_key(1, 5, 5, 5));
    }
}
