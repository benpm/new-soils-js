//! A write-back page cache over [`crate::paged`] files.
//!
//! This is the chunk cache's policy, generalised. `world.rs` already runs it
//! for voxels — probe a memoised pointer table, inflate the block into memory,
//! mutate in RAM, mark dirty, and write out only on a flush interval, on
//! eviction, or at shutdown. Nothing about that is voxel-shaped, so it lives
//! here and chunk *data* (chests and whatever else grows state a `u8` voxel id
//! cannot carry) is the second customer.
//!
//! # Addressing
//!
//! A page is one chunk's worth of a structure. It is addressed exactly as the
//! chunk's voxels are: region file, slot within the region. So `r_0_0_0.bin`
//! slot 37 and `b_0_0_0.bin` slot 37 are the same chunk seen two ways, and a
//! caller that knows where a chunk lives already knows where its block data
//! lives — no index, no id allocation, no second key space to keep coherent
//! with the first.
//!
//! # Residency
//!
//! * A miss reads the region's pointer table (memoised — one file open per
//!   region, not per page) and inflates the block if the entry points at one.
//!   [`ABSENT`](crate::paged::ABSENT) is the common case and costs a map
//!   lookup, which is what makes it acceptable to ask "does this block have
//!   data?" on any block break.
//! * A page stays resident, dirty, until [`take_dirty`](Store::take_dirty)
//!   hands its bytes to the background writer. Writes are never on the caller's
//!   thread.
//! * [`tick_lifecycle`](Store::tick_lifecycle) evicts pages that have been
//!   unpinned and idle for a TTL, flushing them on the way out.
//! * [`pin`](Store::pin)/[`unpin`](Store::unpin) hold a page against eviction
//!   while something is actively using it — an open chest, say, whose viewers
//!   would otherwise be reading a page that vanished under them.
//!
//! An empty value is not stored: it clears its slot back to `ABSENT`, so a
//! chest broken and mined out leaves no trace to reload and no bytes to
//! compact.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use glam::IVec3;

use crate::paged;
use crate::region;

/// How a value crosses the disk boundary. Deliberately byte-oriented rather
/// than `serde`-bound: `paged` compresses, and the store never inspects a
/// payload it did not encode.
pub trait Codec: Sized {
    fn encode(&self) -> Vec<u8>;
    /// `None` on a payload this build cannot read. The store treats that as an
    /// absent page rather than a panic — a format change must not make a world
    /// unopenable — and logs it once per page.
    fn decode(bytes: &[u8]) -> Option<Self>;
    /// Holds nothing worth a block on disk. Such a page clears its slot.
    fn is_empty(&self) -> bool;
}

struct Page<V> {
    value: V,
    dirty: bool,
    pins: u32,
    /// Set while unpinned; the eviction timer runs from it.
    idle_since: Option<Instant>,
}

/// Counters, for tests and the `/stats` debug line. A cache with no visible
/// hit rate is a cache nobody can tell is broken.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StoreStats {
    /// Lookups served from memory.
    pub hits: u64,
    /// Lookups that had to consult the pointer table.
    pub misses: u64,
    /// Misses that actually inflated a block (the rest were `ABSENT`).
    pub loads: u64,
    /// Pages handed to the writer.
    pub writes: u64,
    /// Pages dropped from memory.
    pub evictions: u64,
    /// Payloads this build could not decode.
    pub corrupt: u64,
}

/// One page's pending write: `None` bytes means "clear the slot", not "store
/// nothing" — the distinction [`paged`] draws between `ABSENT` and `SENTINEL`.
pub struct Write {
    pub path: PathBuf,
    pub slot: usize,
    pub bytes: Option<Vec<u8>>,
}

pub struct Store<V> {
    dir: PathBuf,
    prefix: &'static str,
    pages: HashMap<IVec3, Page<V>>,
    /// Memoised pointer tables. `None` = the file does not exist, i.e. nothing
    /// in that region has ever been written.
    ///
    /// Coherent with the background writer for the same reason the chunk
    /// header cache is: an entry is only consulted for a page *not* resident,
    /// and the writer only ever rewrites entries for pages that are — disjoint
    /// sets, provided eviction drops the region's memo, which it does.
    headers: HashMap<PathBuf, Option<paged::Header>>,
    stats: StoreStats,
}

impl<V: Codec + Default> Store<V> {
    /// `prefix` names the structure in its filenames (`b` = block data).
    pub fn new(dir: PathBuf, prefix: &'static str) -> Self {
        Self { dir, prefix, pages: HashMap::new(), headers: HashMap::new(), stats: Default::default() }
    }

    fn path(&self, pos: IVec3) -> PathBuf {
        region::paged_path(&self.dir, self.prefix, pos)
    }

    /// Bring `pos` into memory if it is not already there. Returns whether the
    /// page holds anything.
    fn fault_in(&mut self, pos: IVec3) {
        if self.pages.contains_key(&pos) {
            self.stats.hits += 1;
            return;
        }
        self.stats.misses += 1;
        let path = self.path(pos);
        let entry = {
            let dir = &self.dir;
            let prefix = self.prefix;
            let header = self
                .headers
                .entry(path.clone())
                .or_insert_with(|| paged::read_header(&region::paged_path(dir, prefix, pos)).unwrap_or(None));
            header.as_ref().map(|h| h[region::header_index(pos)])
        };
        let value = match entry {
            Some(e) => match paged::read_block(&path, e) {
                Ok(Some(bytes)) if !bytes.is_empty() => {
                    self.stats.loads += 1;
                    match V::decode(&bytes) {
                        Some(v) => v,
                        None => {
                            self.stats.corrupt += 1;
                            eprintln!("store: undecodable {} page at {pos}, treating as empty", self.prefix);
                            V::default()
                        }
                    }
                }
                Ok(_) => V::default(),
                Err(e) => {
                    eprintln!("store: failed to read {} page at {pos}: {e}", self.prefix);
                    V::default()
                }
            },
            None => V::default(),
        };
        self.pages.insert(pos, Page { value, dirty: false, pins: 0, idle_since: Some(Instant::now()) });
    }

    /// The page at `pos`, loading it if necessary. An unwritten page reads as
    /// `V::default()` — "nothing here" and "nothing yet" are the same answer to
    /// a reader.
    pub fn get(&mut self, pos: IVec3) -> &V {
        self.fault_in(pos);
        &self.pages[&pos].value
    }

    /// As [`get`](Self::get), but marks the page dirty. Taking this handle is
    /// the commitment to a write, so callers that only *might* change something
    /// should check with `get` first.
    pub fn get_mut(&mut self, pos: IVec3) -> &mut V {
        self.fault_in(pos);
        let page = self.pages.get_mut(&pos).expect("faulted in above");
        page.dirty = true;
        &mut page.value
    }

    /// Hold a page in memory. Balanced by [`unpin`](Self::unpin).
    pub fn pin(&mut self, pos: IVec3) {
        self.fault_in(pos);
        let page = self.pages.get_mut(&pos).expect("faulted in above");
        page.pins += 1;
        page.idle_since = None;
    }

    pub fn unpin(&mut self, pos: IVec3) {
        if let Some(page) = self.pages.get_mut(&pos) {
            page.pins = page.pins.saturating_sub(1);
            if page.pins == 0 {
                page.idle_since = Some(Instant::now());
            }
        }
    }

    /// Every dirty page's encoded bytes, clearing the dirty flags. The caller
    /// hands these to the background writer; nothing here touches disk.
    pub fn take_dirty(&mut self) -> Vec<Write> {
        let mut out = Vec::new();
        for (&pos, page) in self.pages.iter_mut() {
            if !page.dirty {
                continue;
            }
            page.dirty = false;
            out.push(Write {
                path: region::paged_path(&self.dir, self.prefix, pos),
                slot: region::header_index(pos),
                bytes: (!page.value.is_empty()).then(|| page.value.encode()),
            });
        }
        self.stats.writes += out.len() as u64;
        out
    }

    /// Drop pages unpinned and idle for longer than `ttl`, flushing the dirty
    /// ones on the way out. Returns their writes, to be enqueued alongside
    /// [`take_dirty`](Self::take_dirty)'s.
    pub fn tick_lifecycle(&mut self, ttl: Duration) -> Vec<Write> {
        let expired: Vec<IVec3> = self
            .pages
            .iter()
            .filter(|(_, p)| p.pins == 0 && p.idle_since.is_some_and(|t| t.elapsed() >= ttl))
            .map(|(&pos, _)| pos)
            .collect();
        let mut out = Vec::new();
        for pos in expired {
            self.evict_into(pos, &mut out);
        }
        self.stats.writes += out.len() as u64;
        out
    }

    /// Drop one page now regardless of its timer, flushing it if dirty. Used
    /// when the chunk it belongs to leaves memory: block data outliving its
    /// voxels is just a leak with extra steps.
    pub fn evict(&mut self, pos: IVec3) -> Vec<Write> {
        let mut out = Vec::new();
        if self.pages.get(&pos).is_some_and(|p| p.pins == 0) {
            self.evict_into(pos, &mut out);
            self.stats.writes += out.len() as u64;
        }
        out
    }

    fn evict_into(&mut self, pos: IVec3, out: &mut Vec<Write>) {
        let Some(page) = self.pages.remove(&pos) else { return };
        self.stats.evictions += 1;
        let path = region::paged_path(&self.dir, self.prefix, pos);
        if page.dirty {
            out.push(Write {
                path: path.clone(),
                slot: region::header_index(pos),
                bytes: (!page.value.is_empty()).then(|| page.value.encode()),
            });
        }
        // The writer is about to rewrite this region's table; the memo is stale
        // the moment that lands.
        self.headers.remove(&path);
    }

    pub fn stats(&self) -> StoreStats {
        self.stats
    }

    /// Pages currently in memory.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    #[cfg(test)]
    pub fn resident(&self) -> usize {
        self.pages.len()
    }
}

/// Apply a batch of [`Write`]s. Called on the background writer thread, never
/// on the tick.
pub fn apply(writes: &[Write]) {
    use std::collections::HashMap;
    let mut by_path: HashMap<&PathBuf, (Vec<paged::Put<'_>>, Vec<usize>)> = HashMap::new();
    for w in writes {
        let (puts, clears) = by_path.entry(&w.path).or_default();
        match &w.bytes {
            Some(bytes) => puts.push(paged::Put { slot: w.slot, payload: Some(bytes), flag: false }),
            None => clears.push(w.slot),
        }
    }
    for (path, (puts, clears)) in by_path {
        if let Err(e) = paged::write_many(path, &puts) {
            eprintln!("store writer: failed to write {path:?}: {e}");
            continue;
        }
        if let Err(e) = paged::clear(path, &clears) {
            eprintln!("store writer: failed to clear slots in {path:?}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial value: a string, so a test can tell pages apart by content.
    #[derive(Default, PartialEq, Debug, Clone)]
    struct Note(String);

    impl Codec for Note {
        fn encode(&self) -> Vec<u8> {
            self.0.clone().into_bytes()
        }
        fn decode(bytes: &[u8]) -> Option<Self> {
            String::from_utf8(bytes.to_vec()).ok().map(Note)
        }
        fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soils-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn store(dir: &PathBuf) -> Store<Note> {
        Store::new(dir.clone(), "t")
    }

    #[test]
    fn a_page_survives_a_flush_and_a_cold_reopen() {
        let d = dir("roundtrip");
        let pos = IVec3::new(3, -2, 40);

        let mut s = store(&d);
        s.get_mut(pos).0 = "chest contents".into();
        apply(&s.take_dirty());

        // Same store, page still resident: served from memory.
        assert_eq!(s.get(pos).0, "chest contents");

        // A fresh store shares nothing but the directory.
        let mut cold = store(&d);
        assert_eq!(cold.get(pos).0, "chest contents");
        assert_eq!(cold.stats().loads, 1, "cold read must actually hit the file");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// The specific way a cache like this goes wrong: a page dropped from
    /// memory before its bytes were written out.
    #[test]
    fn eviction_writes_a_dirty_page_out_rather_than_losing_it() {
        let d = dir("evict");
        let pos = IVec3::new(1, 1, 1);

        let mut s = store(&d);
        s.get_mut(pos).0 = "unflushed".into();
        let writes = s.tick_lifecycle(Duration::ZERO);
        assert_eq!(s.resident(), 0, "an idle page is evicted");
        assert_eq!(writes.len(), 1, "and its bytes come out with it");
        apply(&writes);

        assert_eq!(store(&d).get(pos).0, "unflushed");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pinned_page_is_not_evicted_until_it_is_released() {
        let d = dir("pin");
        let pos = IVec3::new(0, 0, 0);
        let mut s = store(&d);
        s.get_mut(pos).0 = "open chest".into();
        s.pin(pos);

        assert!(s.tick_lifecycle(Duration::ZERO).is_empty());
        assert_eq!(s.resident(), 1, "a pinned page stays put");

        s.unpin(pos);
        assert_eq!(s.tick_lifecycle(Duration::ZERO).len(), 1);
        assert_eq!(s.resident(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_emptied_page_clears_its_slot_instead_of_storing_nothing() {
        let d = dir("clear");
        let pos = IVec3::new(2, 2, 2);
        let mut s = store(&d);
        s.get_mut(pos).0 = "temporary".into();
        apply(&s.take_dirty());
        assert_eq!(store(&d).get(pos).0, "temporary");

        s.get_mut(pos).0.clear();
        let writes = s.take_dirty();
        assert!(writes[0].bytes.is_none(), "an empty value clears rather than writes");
        apply(&writes);

        let mut cold = store(&d);
        assert_eq!(cold.get(pos), &Note::default());
        assert_eq!(cold.stats().loads, 0, "there is nothing left on disk to inflate");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Every page in a region shares one pointer table, and reading it is the
    /// expensive part. Ten pages in one region must not cost ten file opens.
    #[test]
    fn the_pointer_table_is_read_once_per_region_not_once_per_page() {
        let d = dir("headers");
        let mut s = store(&d);
        for i in 0..10 {
            let _ = s.get(IVec3::new(i, 0, 0));
        }
        assert_eq!(s.stats().misses, 10);
        assert_eq!(s.stats().loads, 0, "nothing was ever written, so nothing inflates");
        // A second pass is pure memory.
        for i in 0..10 {
            let _ = s.get(IVec3::new(i, 0, 0));
        }
        assert_eq!(s.stats().hits, 10);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two pages in one region must not overwrite each other — the failure
    /// mode if the slot mapping ignored the position's low bits.
    #[test]
    fn neighbouring_pages_in_one_region_stay_distinct() {
        let d = dir("neighbours");
        let mut s = store(&d);
        let a = IVec3::new(1, 2, 3);
        let b = IVec3::new(2, 2, 3);
        s.get_mut(a).0 = "a".into();
        s.get_mut(b).0 = "b".into();
        apply(&s.take_dirty());

        let mut cold = store(&d);
        assert_eq!(cold.get(a).0, "a");
        assert_eq!(cold.get(b).0, "b");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_undecodable_payload_reads_as_empty_rather_than_panicking() {
        let d = dir("corrupt");
        let pos = IVec3::new(4, 0, 0);
        // Write bytes that are valid for `Note` but not for `Strict`.
        let mut s = store(&d);
        s.get_mut(pos).0 = "not a number".into();
        apply(&s.take_dirty());

        #[derive(Default, PartialEq, Debug)]
        struct Strict(u32);
        impl Codec for Strict {
            fn encode(&self) -> Vec<u8> {
                self.0.to_le_bytes().to_vec()
            }
            fn decode(bytes: &[u8]) -> Option<Self> {
                bytes.try_into().ok().map(|b| Strict(u32::from_le_bytes(b)))
            }
            fn is_empty(&self) -> bool {
                self.0 == 0
            }
        }
        let mut strict: Store<Strict> = Store::new(d.clone(), "t");
        assert_eq!(strict.get(pos), &Strict::default());
        assert_eq!(strict.stats().corrupt, 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
