//! The on-disk format shared by every persisted world structure: a slotted,
//! append-and-repoint file.
//!
//! This is `region.rs`'s file layout, lifted out so more than chunks can use
//! it. A file is a fixed table of `SLOTS` little-endian `u32` entries followed
//! by data blocks:
//!
//! ```text
//! [ 4096 x u32 entry ][ u32 len ][ zlib payload ][ u32 len ][ zlib payload ] ...
//!   ^ the pointer table                ^ a block, addressed by a table entry
//! ```
//!
//! An entry is [`ABSENT`] (nothing here), [`SENTINEL`] (present, zero-length —
//! chunks use it for all-Air), or a byte offset to a block. The high bit is a
//! caller-defined [`FLAG`]; offsets stay far below 2 GB (a full file is around
//! 130 MB plus bounded leaks), so the bit is free. Every offset read masks it
//! off via [`entry_kind`].
//!
//! Writes never rewrite in place: a changed slot appends a fresh block and
//! repoints its entry. That is crash-safe — a torn append leaves an
//! unreferenced block and the 4-byte entry write is the commit — at the cost of
//! leaked space, which [`compact`] reclaims.
//!
//! The *addressing* is the caller's. `region.rs` maps a chunk position to a
//! slot; [`crate::store`] reuses that exact mapping, so a chunk's voxels and
//! its block data sit at the same slot index in two parallel files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

/// Entries per file. 16^3, because the first caller groups 16x16x16 chunks into
/// one region.
pub const SLOTS: usize = 4096;
pub const HEADER_BYTES: u64 = (SLOTS * 4) as u64;

/// Nothing has ever been written to this slot.
pub const ABSENT: u32 = 0;
/// Present, but the payload is empty. Distinct from [`ABSENT`]: "known to hold
/// nothing" is not "unknown", and only the former stops a caller regenerating.
pub const SENTINEL: u32 = 1;
/// Caller-defined bit on an entry (chunks use it for "ever edited").
pub const FLAG: u32 = 0x8000_0000;

/// A file's whole pointer table, read in one shot.
pub type Header = Box<[u32; SLOTS]>;

/// An entry with the caller's flag masked off.
#[inline]
pub fn entry_kind(entry: u32) -> u32 {
    entry & !FLAG
}

#[inline]
pub fn entry_flag(entry: u32) -> bool {
    entry & FLAG != 0
}

/// Read the whole pointer table. `Ok(None)` means the file does not exist (or
/// is too short to hold a table) — nothing in it was ever persisted. Callers
/// memoise this so a per-slot probe costs a map lookup, not a file open.
pub fn read_header(path: &Path) -> io::Result<Option<Header>> {
    let mut file = match File::open(path) {
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
    let mut header = Box::new([0u32; SLOTS]);
    for (i, slot) in header.iter_mut().enumerate() {
        *slot = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    }
    Ok(Some(header))
}

/// Inflate the block an already-known `entry` points at. `Ok(None)` for
/// [`ABSENT`]; `Ok(Some(vec![]))` for [`SENTINEL`]. Only opens the file for a
/// real block, which is why the header is read separately.
pub fn read_block(path: &Path, entry: u32) -> io::Result<Option<Vec<u8>>> {
    match entry_kind(entry) {
        ABSENT => Ok(None),
        SENTINEL => Ok(Some(Vec::new())),
        offset => {
            let mut file = File::open(path)?;
            file.seek(SeekFrom::Start(offset as u64))?;
            let mut len = [0u8; 4];
            file.read_exact(&mut len)?;
            let len = u32::from_le_bytes(len) as u64;
            // Bound the length against the file before allocating for it.
            // `compact` already checks the same field against the mapped
            // length; this path did not, so a truncated or corrupt region
            // could ask for a 4 GB `vec![0u8; ...]` before the `read_exact`
            // that would have failed. `read_exact` reports the corruption
            // either way — the point is to report it without the allocation.
            let avail = file.metadata()?.len().saturating_sub(offset as u64 + 4);
            if len > avail {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("block at {offset} claims {len} bytes, {avail} remain"),
                ));
            }
            let mut compressed = vec![0u8; len as usize];
            file.read_exact(&mut compressed)?;
            let mut out = Vec::new();
            ZlibDecoder::new(&compressed[..]).read_to_end(&mut out)?;
            Ok(Some(out))
        }
    }
}

/// One slot's new contents. A `payload` of `None` (or empty) writes the
/// [`SENTINEL`]; use [`clear`] to return a slot to [`ABSENT`].
pub struct Put<'a> {
    pub slot: usize,
    pub payload: Option<&'a [u8]>,
    pub flag: bool,
}

/// Apply many slot writes to one file, opening it once: append every new block,
/// then repoint each entry.
pub fn write_many(path: &Path, puts: &[Put<'_>]) -> io::Result<()> {
    if puts.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Never truncate: these files are updated in place.
    let mut file =
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    if file.metadata()?.len() < HEADER_BYTES {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&vec![0u8; HEADER_BYTES as usize])?;
    }

    for put in puts {
        debug_assert!(put.slot < SLOTS);
        let mut entry = match put.payload {
            None => SENTINEL,
            Some(raw) if raw.is_empty() => SENTINEL,
            Some(raw) => {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(raw)?;
                let compressed = encoder.finish()?;
                let offset = file.seek(SeekFrom::End(0))?;
                file.write_all(&(compressed.len() as u32).to_le_bytes())?;
                file.write_all(&compressed)?;
                offset as u32
            }
        };
        if put.flag {
            entry |= FLAG;
        }
        write_entry(&mut file, put.slot, entry)?;
    }
    file.flush()
}

/// Return slots to [`ABSENT`] — a header-only write, so the blocks they pointed
/// at leak until the next [`compact`]. A missing file has nothing to clear, and
/// creating one to say so would be worse than doing nothing.
pub fn clear(path: &Path, slots: &[usize]) -> io::Result<()> {
    if slots.is_empty() {
        return Ok(());
    }
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if file.metadata()?.len() < HEADER_BYTES {
        return Ok(());
    }
    for &slot in slots {
        write_entry(&mut file, slot, ABSENT)?;
    }
    file.flush()
}

/// Overwrite entries directly, leaving the blocks alone. For reclassification
/// sweeps that only move the [`FLAG`].
pub fn write_entries(path: &Path, entries: &[(usize, u32)]) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    for &(slot, entry) in entries {
        write_entry(&mut file, slot, entry)?;
    }
    file.flush()
}

fn write_entry(file: &mut File, slot: usize, entry: u32) -> io::Result<()> {
    file.seek(SeekFrom::Start((slot * 4) as u64))?;
    file.write_all(&entry.to_le_bytes())
}

/// Fraction of a file's data bytes that must be dead before it is rewritten.
const COMPACT_LEAK_RATIO: f64 = 0.25;
/// And at least this much absolute waste — tiny files are not worth the churn.
const COMPACT_MIN_LEAK: u64 = 64 * 1024;

/// Rewrite `path` keeping only the blocks `keep` accepts, if enough bytes
/// leaked to be worth it. `keep` receives the raw entry (flag included) and
/// decides whether that slot survives; rejecting one prunes it to [`ABSENT`]
/// and counts its bytes as reclaimable, which is how chunks drop pristine
/// terrain they can simply regenerate.
///
/// Crash-safe: the rebuilt image lands in a temp file that atomically replaces
/// the original.
pub fn compact(path: &Path, keep: impl Fn(u32) -> bool) -> io::Result<()> {
    let data = fs::read(path)?;
    if data.len() < HEADER_BYTES as usize {
        return Ok(());
    }
    let entry_at = |i: usize| u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "block out of bounds");

    let mut blocks: Vec<(usize, usize, usize, u32)> = Vec::new(); // (slot, start, len, flag)
    let mut live: u64 = 0;
    for i in 0..SLOTS {
        let e = entry_at(i);
        if entry_kind(e) <= SENTINEL {
            continue;
        }
        let start = entry_kind(e) as usize;
        let len_bytes: [u8; 4] = data.get(start..start + 4).ok_or_else(bad)?.try_into().unwrap();
        let len = 4 + u32::from_le_bytes(len_bytes) as usize;
        if start + len > data.len() {
            return Err(bad());
        }
        if !keep(e) {
            continue; // pruned: its bytes count as reclaimable
        }
        blocks.push((i, start, len, e & FLAG));
        live += len as u64;
    }
    let total = data.len() as u64 - HEADER_BYTES;
    let leaked = total.saturating_sub(live);
    if leaked < COMPACT_MIN_LEAK || (leaked as f64) < COMPACT_LEAK_RATIO * total as f64 {
        return Ok(());
    }

    let mut out = vec![0u8; HEADER_BYTES as usize];
    for i in 0..SLOTS {
        let e = entry_at(i);
        // Sentinels carry no block, so they survive iff `keep` wants them.
        if entry_kind(e) == SENTINEL && keep(e) {
            out[i * 4..i * 4 + 4].copy_from_slice(&e.to_le_bytes());
        }
    }
    for (slot, start, len, flag) in blocks {
        let new_off = out.len() as u32 | flag;
        out.extend_from_slice(&data[start..start + len]);
        out[slot * 4..slot * 4 + 4].copy_from_slice(&new_off.to_le_bytes());
    }
    let tmp = path.with_extension("bin.tmp");
    fs::write(&tmp, &out)?;
    fs::rename(&tmp, path)?;
    println!("paged file compacted {path:?}: reclaimed {leaked} bytes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("soils-paged-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("f.bin")
    }

    fn slot(path: &Path, i: usize) -> Option<Vec<u8>> {
        let h = read_header(path).ok().flatten()?;
        read_block(path, h[i]).unwrap()
    }

    #[test]
    fn absent_sentinel_and_payload_are_three_distinct_states() {
        let p = tmp("states");
        assert!(read_header(&p).unwrap().is_none(), "no file, no header");
        write_many(
            &p,
            &[
                Put { slot: 1, payload: Some(b"hello"), flag: false },
                Put { slot: 2, payload: None, flag: false },
            ],
        )
        .unwrap();

        assert_eq!(slot(&p, 1).as_deref(), Some(&b"hello"[..]));
        assert_eq!(slot(&p, 2), Some(Vec::new()), "sentinel reads as an empty payload");
        assert_eq!(slot(&p, 3), None, "untouched slot is absent");
    }

    #[test]
    fn a_rewrite_repoints_the_entry_and_a_clear_returns_it_to_absent() {
        let p = tmp("rewrite");
        write_many(&p, &[Put { slot: 7, payload: Some(b"first"), flag: false }]).unwrap();
        write_many(&p, &[Put { slot: 7, payload: Some(b"second"), flag: true }]).unwrap();
        assert_eq!(slot(&p, 7).as_deref(), Some(&b"second"[..]));
        let h = read_header(&p).unwrap().unwrap();
        assert!(entry_flag(h[7]), "the flag rides on the entry");

        clear(&p, &[7]).unwrap();
        assert_eq!(slot(&p, 7), None);
        // Clearing a file that does not exist is a no-op, not an error.
        clear(&p.with_file_name("missing.bin"), &[7]).unwrap();
    }

    #[test]
    fn compaction_reclaims_leaks_and_honours_the_keep_predicate() {
        let p = tmp("compact");
        // Incompressible payloads, so the file actually grows past the leak floor.
        let noise = |n: u64| {
            let mut s = n.wrapping_add(1);
            (0..40_000u32)
                .map(|_| {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (s >> 33) as u8
                })
                .collect::<Vec<u8>>()
        };
        write_many(&p, &[Put { slot: 5, payload: None, flag: true }]).unwrap();
        write_many(&p, &[Put { slot: 6, payload: Some(&noise(9)), flag: false }]).unwrap();
        for round in 0..10u64 {
            write_many(&p, &[Put { slot: 4, payload: Some(&noise(round)), flag: true }]).unwrap();
        }
        let before = fs::metadata(&p).unwrap().len();

        compact(&p, entry_flag).unwrap();

        let after = fs::metadata(&p).unwrap().len();
        assert!(after < before / 3, "9 superseded blocks + 1 rejected one ({before} -> {after})");
        assert_eq!(slot(&p, 4).as_deref(), Some(&noise(9)[..]), "the live block survives intact");
        assert_eq!(slot(&p, 5), Some(Vec::new()), "a kept sentinel survives");
        assert_eq!(slot(&p, 6), None, "a rejected slot prunes to absent");

        // Idempotent: nothing left to reclaim.
        compact(&p, entry_flag).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().len(), after);
    }
}
