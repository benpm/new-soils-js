//! Chunk persistence: the typed layer over [`crate::paged`].
//!
//! Chunks are grouped into 16x16x16 regions, one `r_<x>_<y>_<z>.bin` per
//! region. The file format — a 4096-entry pointer table, append-and-repoint
//! writes, zlib blocks, temp-file compaction — lives in `paged`; everything
//! here is the chunk-shaped part of it: which slot a position maps to, that an
//! all-Air chunk stores as the sentinel rather than 32 KB of zeroes, and that a
//! *pristine* chunk is worth pruning because worldgen can reproduce it exactly.
//!
//! The paged [`FLAG`](crate::paged::FLAG) is this layer's "ever edited" bit.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use glam::IVec3;
use soils_protocol::{CHUNK_CUBED, REGION_SIZE, ChunkVolume};

use crate::paged::{self, Put};

const REGION_BITS: i32 = 4; // log2(16)
const REGION_MASK: i32 = REGION_SIZE - 1; // 15

pub(crate) const EDITED_FLAG: u32 = paged::FLAG;

#[inline]
pub(crate) fn entry_edited(entry: u32) -> bool {
    paged::entry_flag(entry)
}

/// Path of the file holding `pos`'s data for a given kind of world structure.
/// `prefix` names the structure (`r` = chunk voxels, `b` = block data), so
/// parallel files share one addressing scheme: a chunk's voxels and its block
/// data live at the same slot index in `r_*.bin` and `b_*.bin`.
pub(crate) fn paged_path(dir: &Path, prefix: &str, pos: IVec3) -> PathBuf {
    dir.join(format!(
        "{prefix}_{}_{}_{}.bin",
        pos.x >> REGION_BITS,
        pos.y >> REGION_BITS,
        pos.z >> REGION_BITS
    ))
}

pub(crate) fn region_path(dir: &Path, pos: IVec3) -> PathBuf {
    paged_path(dir, "r", pos)
}

/// Index of this chunk's entry within its file's 4096-entry table.
pub(crate) fn header_index(pos: IVec3) -> usize {
    let lx = (pos.x & REGION_MASK) as usize;
    let ly = (pos.y & REGION_MASK) as usize;
    let lz = (pos.z & REGION_MASK) as usize;
    ((ly + lz * REGION_SIZE as usize) * REGION_SIZE as usize) + lx
}

/// Read a region file's whole pointer table. `Ok(None)` if nothing in that
/// region has ever been persisted. Callers memoise this so a per-chunk probe
/// becomes an in-memory lookup instead of a file open each.
pub(crate) fn read_header(dir: &Path, pos: IVec3) -> io::Result<Option<paged::Header>> {
    paged::read_header(&region_path(dir, pos))
}

/// Resolve a single chunk given its already-known header `entry` (see
/// [`read_header`]). Only opens the region file for a present, non-empty block.
pub(crate) fn read_chunk(dir: &Path, pos: IVec3, entry: u32) -> io::Result<Option<ChunkVolume>> {
    let Some(bytes) = paged::read_block(&region_path(dir, pos), entry)? else { return Ok(None) };
    if bytes.is_empty() {
        return Ok(Some(ChunkVolume::empty()));
    }
    if bytes.len() != CHUNK_CUBED {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "decompressed chunk has wrong size"));
    }
    Ok(Some(ChunkVolume::from_bytes(&bytes)))
}

/// Load a chunk from its region file. `Ok(None)` if it has never been persisted
/// (the caller should then generate it). The read path uses the cached
/// [`read_header`] + [`read_chunk`] split directly; this whole-in-one helper is
/// kept for tests and one-off callers.
#[allow(dead_code)]
pub fn load(dir: &Path, pos: IVec3) -> io::Result<Option<ChunkVolume>> {
    let Some(header) = read_header(dir, pos)? else { return Ok(None) };
    read_chunk(dir, pos, header[header_index(pos)])
}

/// Persist a single chunk. Thin wrapper over [`save_many`]; kept for tests.
#[allow(dead_code)]
pub fn save(dir: &Path, pos: IVec3, volume: &ChunkVolume, edited: bool) -> io::Result<()> {
    save_many(dir, &[(pos, volume, edited)])
}

/// Persist many chunks at once, opening each region file only once. This is
/// what the background writer uses to coalesce a fresh-world burst (hundreds of
/// chunks over a handful of region files) into a few file writes.
pub fn save_many(dir: &Path, chunks: &[(IVec3, &ChunkVolume, bool)]) -> io::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;

    let mut by_region: HashMap<PathBuf, Vec<Put<'_>>> = HashMap::new();
    for &(pos, vol, edited) in chunks {
        // An all-Air chunk stores as the sentinel: present, but no payload.
        let payload = (!vol.is_empty()).then(|| vol.as_bytes());
        by_region
            .entry(region_path(dir, pos))
            .or_default()
            .push(Put { slot: header_index(pos), payload, flag: edited });
    }
    for (path, puts) in by_region {
        paged::write_many(&path, &puts)?;
    }
    Ok(())
}

/// Reclassify every persisted chunk in `dir`: `classify` receives one region's
/// worth of `(pos, volume)` at a time and returns the edited flag for each.
/// Header-only rewrites, one pass per file. Used by the world-open migration
/// sweep ("pristine <=> bytes equal current gen") — see `World::new`.
pub(crate) fn classify_dir(
    dir: &Path,
    mut classify: impl FnMut(&[(IVec3, ChunkVolume)]) -> Vec<bool>,
) -> io::Result<()> {
    for path in region_files(dir) {
        let Some(rpos) = parse_region_name(&path) else { continue };
        let base = rpos * REGION_SIZE;
        let Some(header) = paged::read_header(&path)? else { continue };

        let mut present: Vec<(IVec3, ChunkVolume)> = Vec::new();
        let mut indices: Vec<usize> = Vec::new();
        for (i, &e) in header.iter().enumerate() {
            if paged::entry_kind(e) == paged::ABSENT {
                continue;
            }
            let local = IVec3::new(
                (i as i32) & REGION_MASK,
                ((i as i32) >> REGION_BITS) & REGION_MASK,
                (i as i32) >> (REGION_BITS * 2),
            );
            let pos = base + local;
            let vol = read_chunk(dir, pos, e)?.expect("present entry");
            present.push((pos, vol));
            indices.push(i);
        }
        if present.is_empty() {
            continue;
        }
        let flags = classify(&present);
        assert_eq!(flags.len(), present.len());

        let updates: Vec<(usize, u32)> = indices
            .iter()
            .zip(&flags)
            .map(|(&i, &edited)| {
                (i, (header[i] & !EDITED_FLAG) | if edited { EDITED_FLAG } else { 0 })
            })
            .filter(|&(i, e)| e != header[i])
            .collect();
        paged::write_entries(&path, &updates)?;
    }
    Ok(())
}

/// Every `.bin` file in `dir` — both region and block-data files, since they
/// share a format and a compaction policy differs only in the `keep` rule.
fn paged_files(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "bin")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&format!("{prefix}_")))
        })
        .collect()
}

fn region_files(dir: &Path) -> Vec<PathBuf> {
    paged_files(dir, "r")
}

/// Region coordinates from a `r_<x>_<y>_<z>.bin` filename.
fn parse_region_name(path: &Path) -> Option<IVec3> {
    let stem = path.file_stem()?.to_str()?;
    let mut it = stem.strip_prefix("r_")?.split('_');
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let z = it.next()?.parse().ok()?;
    Some(IVec3::new(x, y, z))
}

/// Compact every persisted file in `dir` whose leaked-byte share crosses the
/// threshold. Called on world open; a corrupt or in-use file is skipped, never
/// fatal.
///
/// Chunks keep only *edited* entries: a pristine chunk is byte-reproducible
/// from the world identity, so pruning it costs a regeneration and saves its
/// bytes. Block data keeps everything — nothing regenerates a chest.
pub fn compact_dir(dir: &Path) {
    for (prefix, keep) in
        [("r", &entry_edited as &dyn Fn(u32) -> bool), ("b", &|_: u32| true as bool)]
    {
        for path in paged_files(dir, prefix) {
            if let Err(e) = paged::compact(&path, keep) {
                eprintln!("compaction skipped {path:?}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_solid_and_empty() {
        let dir = std::env::temp_dir().join(format!("soils-region-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // A solid-ish chunk.
        let mut vol = ChunkVolume::empty();
        vol.set(1, 2, 3, 7);
        vol.set(31, 31, 31, 4);
        let pos = IVec3::new(8, 7, 8);
        save(&dir, pos, &vol, false).unwrap();
        let loaded = load(&dir, pos).unwrap().expect("chunk present");
        assert_eq!(loaded.get(1, 2, 3), 7);
        assert_eq!(loaded.get(31, 31, 31), 4);
        assert_eq!(loaded.get(0, 0, 0), 0);

        // An empty chunk in the same region records the sentinel.
        let epos = IVec3::new(9, 7, 8);
        save(&dir, epos, &ChunkVolume::empty(), false).unwrap();
        assert!(load(&dir, epos).unwrap().unwrap().is_empty());

        // An untouched chunk is absent.
        assert!(load(&dir, IVec3::new(10, 7, 8)).unwrap().is_none());

        // Rewrite repoints the header to fresh data.
        vol.set(5, 5, 5, 9);
        save(&dir, pos, &vol, false).unwrap();
        assert_eq!(load(&dir, pos).unwrap().unwrap().get(5, 5, 5), 9);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_many_coalesces_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("soils-region-many-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let mut a = ChunkVolume::empty();
        a.set(0, 0, 0, 5);
        let mut b = ChunkVolume::empty();
        b.set(31, 31, 31, 6);
        let empty = ChunkVolume::empty();

        // Two chunks in region (0,0,0), one in a neighbouring region, plus an
        // empty chunk — all written in one coalesced call.
        let p_a = IVec3::new(1, 1, 1);
        let p_b = IVec3::new(2, 1, 1);
        let p_neighbour = IVec3::new(16, 1, 1); // region (1,0,0)
        let p_empty = IVec3::new(3, 1, 1);
        save_many(
            &dir,
            &[(p_a, &a, false), (p_b, &b, true), (p_neighbour, &b, false), (p_empty, &empty, false)],
        )
        .unwrap();

        assert_eq!(load(&dir, p_a).unwrap().unwrap().get(0, 0, 0), 5);
        assert_eq!(load(&dir, p_b).unwrap().unwrap().get(31, 31, 31), 6);
        assert_eq!(load(&dir, p_neighbour).unwrap().unwrap().get(31, 31, 31), 6);
        assert!(load(&dir, p_empty).unwrap().unwrap().is_empty());
        // An untouched chunk in a written region is still absent.
        assert!(load(&dir, IVec3::new(4, 1, 1)).unwrap().is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_reclaims_leaked_blocks_and_preserves_content() {
        let dir = std::env::temp_dir().join(format!("soils-region-compact-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // An incompressible chunk (~32 KB compressed), rewritten repeatedly:
        // append-only saves leak every superseded block.
        let mut vol = ChunkVolume::empty();
        let mut s = 1u64;
        for i in 0..soils_protocol::CHUNK_CUBED {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            vol.as_bytes_mut()[i] = (s >> 33) as u8;
        }
        let pos = IVec3::new(1, 1, 1);
        let epos = IVec3::new(2, 1, 1);
        save(&dir, epos, &ChunkVolume::empty(), true).unwrap();
        for round in 0..10 {
            vol.set(0, 0, 0, round);
            save(&dir, pos, &vol, true).unwrap();
        }
        let path = region_path(&dir, pos);
        let bloated = fs::metadata(&path).unwrap().len();

        compact_dir(&dir);

        let compacted = fs::metadata(&path).unwrap().len();
        assert!(
            compacted < bloated / 4,
            "compaction should reclaim ~9 of 10 blocks ({bloated} -> {compacted})"
        );
        // Content preserved: latest rewrite, the empty sentinel, absent chunks.
        assert_eq!(load(&dir, pos).unwrap().unwrap().get(0, 0, 0), 9);
        assert!(load(&dir, epos).unwrap().unwrap().is_empty());
        assert!(load(&dir, IVec3::new(3, 1, 1)).unwrap().is_none());
        // Idempotent: nothing left to reclaim.
        compact_dir(&dir);
        assert_eq!(fs::metadata(&path).unwrap().len(), compacted);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn edited_flag_round_trips_and_survives_compaction() {
        let dir = std::env::temp_dir().join(format!("soils-region-edited-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let mut vol = ChunkVolume::empty();
        vol.set(1, 1, 1, 2);
        let p_edited = IVec3::new(1, 1, 1);
        let p_pristine = IVec3::new(2, 1, 1);
        let p_empty_edited = IVec3::new(3, 1, 1);
        save(&dir, p_edited, &vol, true).unwrap();
        save(&dir, p_pristine, &vol, false).unwrap();
        save(&dir, p_empty_edited, &ChunkVolume::empty(), true).unwrap();

        let flag = |pos| {
            let h = read_header(&dir, pos).unwrap().unwrap();
            entry_edited(h[header_index(pos)])
        };
        assert!(flag(p_edited));
        assert!(!flag(p_pristine));
        assert!(flag(p_empty_edited), "the sentinel must carry the flag too");
        // Volumes still load correctly through the flagged entries.
        assert_eq!(load(&dir, p_edited).unwrap().unwrap().get(1, 1, 1), 2);
        assert!(load(&dir, p_empty_edited).unwrap().unwrap().is_empty());

        // Bloat the file so compaction actually rewrites it, then re-check.
        let mut big = ChunkVolume::empty();
        let mut st = 1u64;
        for i in 0..soils_protocol::CHUNK_CUBED {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            big.as_bytes_mut()[i] = (st >> 33) as u8;
        }
        for round in 0..10 {
            big.set(0, 0, 0, round);
            save(&dir, p_edited, &big, true).unwrap();
        }
        compact_dir(&dir);
        assert!(flag(p_edited));
        assert!(flag(p_empty_edited));
        assert_eq!(load(&dir, p_edited).unwrap().unwrap().get(0, 0, 0), 9);
        // The pristine chunk pruned to ABSENT: it regenerates on demand.
        assert!(
            load(&dir, p_pristine).unwrap().is_none(),
            "pristine chunk should prune out of the compacted file"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_dir_rewrites_flags_from_the_callback() {
        let dir = std::env::temp_dir().join(format!("soils-region-classify-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let mut vol = ChunkVolume::empty();
        vol.set(0, 0, 0, 7);
        // Wrongly-flagged both ways, plus one in a negative-coordinate region.
        let p_a = IVec3::new(1, 1, 1);
        let p_b = IVec3::new(2, 1, 1);
        let p_neg = IVec3::new(-3, 1, 1);
        save(&dir, p_a, &vol, true).unwrap();
        save(&dir, p_b, &vol, false).unwrap();
        save(&dir, p_neg, &vol, false).unwrap();

        // Classify: edited iff the voxel at (0,0,0) is 7 and x is even (an
        // arbitrary rule proving positions + volumes reach the callback).
        classify_dir(&dir, |batch| {
            batch.iter().map(|(pos, v)| v.get(0, 0, 0) == 7 && pos.x % 2 == 0).collect()
        })
        .unwrap();

        let flag = |pos| {
            let h = read_header(&dir, pos).unwrap().unwrap();
            entry_edited(h[header_index(pos)])
        };
        assert!(!flag(p_a));
        assert!(flag(p_b));
        assert!(!flag(p_neg), "odd x in a negative region clears");
        // Content untouched (header-only rewrite).
        assert_eq!(load(&dir, p_neg).unwrap().unwrap().get(0, 0, 0), 7);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_header_and_chunk_match_load() {
        let dir = std::env::temp_dir().join(format!("soils-region-hdr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // Missing region -> no header, and `load` agrees.
        let pos = IVec3::new(1, 2, 3);
        assert!(read_header(&dir, pos).unwrap().is_none());
        assert!(load(&dir, pos).unwrap().is_none());

        let mut vol = ChunkVolume::empty();
        vol.set(7, 8, 9, 3);
        save(&dir, pos, &vol, false).unwrap();

        let header = read_header(&dir, pos).unwrap().expect("header present");
        let via_parts = read_chunk(&dir, pos, header[header_index(pos)]).unwrap();
        let via_load = load(&dir, pos).unwrap();
        assert_eq!(via_parts.map(|v| v.get(7, 8, 9)), Some(3));
        assert_eq!(via_load.map(|v| v.get(7, 8, 9)), Some(3));

        let _ = fs::remove_dir_all(&dir);
    }

    /// The two structures address the same chunk identically, which is the
    /// whole point of `paged_path`: one slot number serves both files.
    #[test]
    fn chunk_voxels_and_block_data_share_an_address() {
        let dir = Path::new("regions");
        let pos = IVec3::new(-17, 3, 40);
        assert_eq!(region_path(dir, pos), paged_path(dir, "r", pos));
        assert_ne!(paged_path(dir, "r", pos), paged_path(dir, "b", pos));
        assert!(header_index(pos) < paged::SLOTS);
    }
}
