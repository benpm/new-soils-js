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

const ABSENT: u32 = 0;
const EMPTY: u32 = 1;

fn region_path(dir: &Path, pos: IVec3) -> PathBuf {
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

/// Load a chunk from its region file. Returns `Ok(None)` if it has never been
/// persisted (the caller should then generate it).
pub fn load(dir: &Path, pos: IVec3) -> io::Result<Option<ChunkVolume>> {
    let path = region_path(dir, pos);
    if !path.exists() {
        return Ok(None);
    }
    let mut file = File::open(&path)?;
    if file.metadata()?.len() < HEADER_BYTES {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(header_offset(pos)))?;
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf)?;
    match u32::from_le_bytes(buf) {
        ABSENT => Ok(None),
        EMPTY => Ok(Some(ChunkVolume::empty())),
        offset => {
            file.seek(SeekFrom::Start(offset as u64))?;
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

/// Persist a chunk to its region file, creating the file/header if needed.
pub fn save(dir: &Path, pos: IVec3, volume: &ChunkVolume) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = region_path(dir, pos);
    let mut file = OpenOptions::new().read(true).write(true).create(true).open(&path)?;

    // Initialize the header on a freshly created file.
    if file.metadata()?.len() < HEADER_BYTES {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&vec![0u8; HEADER_BYTES as usize])?;
    }

    let entry = if volume.is_empty() {
        EMPTY
    } else {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(volume.as_bytes())?;
        let compressed = encoder.finish()?;

        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(&(compressed.len() as u32).to_le_bytes())?;
        file.write_all(&compressed)?;
        offset as u32
    };

    file.seek(SeekFrom::Start(header_offset(pos)))?;
    file.write_all(&entry.to_le_bytes())?;
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
}
