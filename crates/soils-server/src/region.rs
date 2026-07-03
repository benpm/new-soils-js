//! Region-file persistence, a clean Rust take on the JS `Region` class.
//!
//! Chunks are grouped into 16×16×16 regions, one file per region. Each file
//! starts with a header of `16^3` little-endian `u32` pointers (one per chunk):
//! `0` = absent, `1` = present-but-empty (all Air), anything else = a byte
//! offset into the file where the chunk's data block lives. A data block is a
//! `u32` length followed by zlib-compressed voxels.
//!
//! Saves are append-only (like the JS append path): rewriting a chunk appends a
//! fresh block and repoints the header, which is simple and crash-safe at the
//! cost of some wasted space until a future compaction pass.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use glam::IVec3;
use soils_protocol::{CHUNK_CUBED, REGION_SIZE, ChunkVolume};

const REGION_BITS: i32 = 4; // log2(16)
const REGION_MASK: i32 = REGION_SIZE - 1; // 15
const HEADER_ENTRIES: usize = (REGION_SIZE * REGION_SIZE * REGION_SIZE) as usize; // 4096
const HEADER_BYTES: u64 = (HEADER_ENTRIES * 4) as u64; // 16384

pub(crate) const ABSENT: u32 = 0;
pub(crate) const EMPTY: u32 = 1;

pub(crate) fn region_path(dir: &Path, pos: IVec3) -> PathBuf {
    dir.join(format!(
        "r_{}_{}_{}.bin",
        pos.x >> REGION_BITS,
        pos.y >> REGION_BITS,
        pos.z >> REGION_BITS
    ))
}

/// Byte offset of this chunk's header entry within its region file.
fn header_offset(pos: IVec3) -> u64 {
    let lx = (pos.x & REGION_MASK) as u64;
    let ly = (pos.y & REGION_MASK) as u64;
    let lz = (pos.z & REGION_MASK) as u64;
    (((ly + lz * REGION_SIZE as u64) * REGION_SIZE as u64) + lx) * 4
}

/// Index of this chunk's header entry within a region's 4096-entry header,
/// for use with a [`read_header`] snapshot.
pub(crate) fn header_index(pos: IVec3) -> usize {
    (header_offset(pos) / 4) as usize
}

/// Read a region file's full 16 KB header in one shot. Returns `Ok(None)` if the
/// region file doesn't exist (or is too short to hold a header) — i.e. nothing
/// in that region has ever been persisted. Callers memoise this so per-chunk
/// probes become in-memory lookups instead of a file open each.
pub(crate) fn read_header(dir: &Path, pos: IVec3) -> io::Result<Option<Box<[u32; HEADER_ENTRIES]>>> {
    let path = region_path(dir, pos);
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if file.metadata()?.len() < HEADER_BYTES {
        return Ok(None);
    }
    let mut bytes = vec![0u8; HEADER_BYTES as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut bytes)?;
    let mut header = Box::new([0u32; HEADER_ENTRIES]);
    for (i, slot) in header.iter_mut().enumerate() {
        *slot = u32::from_le_bytes([bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2], bytes[i * 4 + 3]]);
    }
    Ok(Some(header))
}

/// Resolve a single chunk given its already-known header `entry` (see
/// [`read_header`]). Only opens the region file for a present, non-empty block.
pub(crate) fn read_chunk(dir: &Path, pos: IVec3, entry: u32) -> io::Result<Option<ChunkVolume>> {
    match entry {
        ABSENT => Ok(None),
        EMPTY => Ok(Some(ChunkVolume::empty())),
        offset => {
            let mut file = File::open(region_path(dir, pos))?;
            file.seek(SeekFrom::Start(offset as u64))?;
            let mut buf = [0u8; 4];
            file.read_exact(&mut buf)?;
            let len = u32::from_le_bytes(buf) as usize;
            let mut compressed = vec![0u8; len];
            file.read_exact(&mut compressed)?;

            let mut voxels = Vec::with_capacity(CHUNK_CUBED);
            ZlibDecoder::new(&compressed[..]).read_to_end(&mut voxels)?;
            if voxels.len() != CHUNK_CUBED {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "decompressed chunk has wrong size",
                ));
            }
            Ok(Some(ChunkVolume::from_bytes(&voxels)))
        }
    }
}

/// Load a chunk from its region file. Returns `Ok(None)` if it has never been
/// persisted (the caller should then generate it). The read path uses the
/// cached [`read_header`] + [`read_chunk`] split directly; this whole-in-one
/// helper is kept for tests and one-off callers.
#[allow(dead_code)]
pub fn load(dir: &Path, pos: IVec3) -> io::Result<Option<ChunkVolume>> {
    let Some(header) = read_header(dir, pos)? else { return Ok(None) };
    read_chunk(dir, pos, header[header_index(pos)])
}

/// Persist a single chunk. Thin wrapper over [`save_many`]; kept for tests.
#[allow(dead_code)]
pub fn save(dir: &Path, pos: IVec3, volume: &ChunkVolume) -> io::Result<()> {
    save_many(dir, &[(pos, volume)])
}

/// Persist many chunks at once, opening each region file only once and applying
/// all of that region's updates before moving on. This is what the background
/// writer uses to coalesce a fresh-world burst (hundreds of chunks spanning a
/// handful of region files) into a few file writes.
pub fn save_many(dir: &Path, chunks: &[(IVec3, &ChunkVolume)]) -> io::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;

    // Group by region file so each is opened/written once.
    let mut by_region: HashMap<PathBuf, Vec<(IVec3, &ChunkVolume)>> = HashMap::new();
    for &(pos, vol) in chunks {
        by_region.entry(region_path(dir, pos)).or_default().push((pos, vol));
    }

    for (path, group) in by_region {
        let mut file = OpenOptions::new().read(true).write(true).create(true).open(&path)?;
        // Initialize the header on a freshly created file.
        if file.metadata()?.len() < HEADER_BYTES {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&vec![0u8; HEADER_BYTES as usize])?;
        }

        for (pos, vol) in group {
            let entry = if vol.is_empty() {
                EMPTY
            } else {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(vol.as_bytes())?;
                let compressed = encoder.finish()?;

                let offset = file.seek(SeekFrom::End(0))?;
                file.write_all(&(compressed.len() as u32).to_le_bytes())?;
                file.write_all(&compressed)?;
                offset as u32
            };
            file.seek(SeekFrom::Start(header_offset(pos)))?;
            file.write_all(&entry.to_le_bytes())?;
        }
        file.flush()?;
    }
    Ok(())
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
        save(&dir, pos, &vol).unwrap();
        let loaded = load(&dir, pos).unwrap().expect("chunk present");
        assert_eq!(loaded.get(1, 2, 3), 7);
        assert_eq!(loaded.get(31, 31, 31), 4);
        assert_eq!(loaded.get(0, 0, 0), 0);

        // An empty chunk in the same region records the EMPTY sentinel.
        let epos = IVec3::new(9, 7, 8);
        save(&dir, epos, &ChunkVolume::empty()).unwrap();
        assert!(load(&dir, epos).unwrap().unwrap().is_empty());

        // An untouched chunk is absent.
        assert!(load(&dir, IVec3::new(10, 7, 8)).unwrap().is_none());

        // Rewrite repoints the header to fresh data.
        vol.set(5, 5, 5, 9);
        save(&dir, pos, &vol).unwrap();
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
            &[(p_a, &a), (p_b, &b), (p_neighbour, &b), (p_empty, &empty)],
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
    fn read_header_and_chunk_match_load() {
        let dir = std::env::temp_dir().join(format!("soils-region-hdr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // Missing region → no header, and `load` agrees.
        let pos = IVec3::new(1, 2, 3);
        assert!(read_header(&dir, pos).unwrap().is_none());
        assert!(load(&dir, pos).unwrap().is_none());

        let mut vol = ChunkVolume::empty();
        vol.set(7, 8, 9, 3);
        save(&dir, pos, &vol).unwrap();

        let header = read_header(&dir, pos).unwrap().expect("header present");
        let via_parts = read_chunk(&dir, pos, header[header_index(pos)]).unwrap();
        let via_load = load(&dir, pos).unwrap();
        assert_eq!(via_parts.map(|v| v.get(7, 8, 9)), Some(3));
        assert_eq!(via_load.map(|v| v.get(7, 8, 9)), Some(3));

        let _ = fs::remove_dir_all(&dir);
    }
}
