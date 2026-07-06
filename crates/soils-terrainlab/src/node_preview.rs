//! Lazy, background per-node output previews.
//!
//! Each value node can show a small grayscale thumbnail of the field it
//! produces. Computing 20-odd 64² fields every frame would stall the editor, so
//! this is decoupled entirely from the render thread:
//!
//! - **Content-addressed:** a node's [`EditorGraph::node_signatures`] hash is
//!   the cache key. It changes iff the node's own params or anything upstream
//!   changed — exactly when its thumbnail would — so dragging one slider only
//!   invalidates that node and its downstream dependents.
//! - **Off-thread:** stale thumbnails are computed in an [`AsyncComputeTaskPool`]
//!   task (pure CPU — no egui/GPU). The render thread only hashes (cheap), looks
//!   up the cache, and uploads finished 64² textures.
//! - **Idle-gated:** a batch is spawned only when the pointer is idle, so there
//!   is zero preview work while you drag, and no churn recomputing signatures
//!   that are about to change again.

use std::collections::{HashMap, HashSet};

use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};
use bevy_egui::egui;
use noise::Simplex;
use soils_worldgen::graph::TerrainGraph;

/// Thumbnail resolution (computed and displayed square).
pub const PREVIEW_N: usize = 64;
/// On-node display size, in points.
pub const THUMB_PX: f32 = 72.0;
/// World span each thumbnail covers, centred on the origin (matches the 2D map).
const PREVIEW_SPAN: f64 = 2048.0;

/// Background thumbnail cache keyed by node content signature.
pub struct NodePreviews {
    pub enabled: bool,
    cache: HashMap<u64, egui::TextureHandle>,
    task: Option<Task<Vec<(u64, Vec<u8>)>>>,
    in_flight: HashSet<u64>,
}

impl Default for NodePreviews {
    fn default() -> Self {
        Self { enabled: true, cache: HashMap::new(), task: None, in_flight: HashSet::new() }
    }
}

impl NodePreviews {
    /// Poll a finished batch (uploading its textures), prune the cache to the
    /// live signatures, and — when the pointer is idle — spawn a background
    /// batch for any value node whose thumbnail isn't cached yet. Returns the
    /// `sig → texture` map the canvas draws from.
    ///
    /// `sigs[i]` is node `i`'s signature; `canon[i]` is its `TerrainGraph` index
    /// (`None` for `Output` sinks, which get no thumbnail).
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        graph: &TerrainGraph,
        seed: u32,
        sigs: &[u64],
        canon: &[Option<usize>],
        pointer_idle: bool,
    ) -> HashMap<u64, egui::TextureId> {
        // 1. Collect a finished batch.
        if let Some(task) = &mut self.task {
            if let Some(done) = block_on(poll_once(task)) {
                self.task = None;
                self.in_flight.clear();
                for (sig, rgba) in done {
                    let img =
                        egui::ColorImage::from_rgba_unmultiplied([PREVIEW_N, PREVIEW_N], &rgba);
                    let tex = ctx.load_texture(
                        format!("np{sig:016x}"),
                        img,
                        egui::TextureOptions::NEAREST,
                    );
                    self.cache.insert(sig, tex);
                }
            }
        }

        // 2. Prune to live value-node signatures (+ anything still computing).
        let live: HashSet<u64> =
            (0..sigs.len()).filter(|&i| canon[i].is_some()).map(|i| sigs[i]).collect();
        self.cache.retain(|k, _| live.contains(k) || self.in_flight.contains(k));

        // 3. Map for the canvas (all entries are live after pruning).
        let map: HashMap<u64, egui::TextureId> =
            self.cache.iter().map(|(sig, tex)| (*sig, tex.id())).collect();

        // 4. Spawn a batch for the still-missing value nodes, only when idle.
        if pointer_idle && self.task.is_none() {
            let mut seen = HashSet::new();
            let stale: Vec<(u64, usize)> = (0..sigs.len())
                .filter_map(|i| {
                    let node = canon[i]?; // value nodes only
                    let sig = sigs[i];
                    if self.cache.contains_key(&sig) || !seen.insert(sig) {
                        return None; // cached, or already queued this batch
                    }
                    Some((sig, node))
                })
                .collect();
            if !stale.is_empty() {
                self.in_flight = stale.iter().map(|(s, _)| *s).collect();
                let graph = graph.clone();
                let pool = AsyncComputeTaskPool::get();
                self.task = Some(pool.spawn(async move {
                    let sim = Simplex::new(seed);
                    stale
                        .into_iter()
                        .map(|(sig, node)| (sig, render_field(&graph, &sim, node)))
                        .collect()
                }));
            }
        }

        map
    }
}

/// Sample a node's field on a `PREVIEW_N`² grid, normalise to its own min→max,
/// and write grayscale RGBA. Pure CPU — runs inside the background task.
fn render_field(graph: &TerrainGraph, sim: &Simplex, node: usize) -> Vec<u8> {
    let n = PREVIEW_N;
    let mut vals = vec![0f64; n * n];
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for j in 0..n {
        for i in 0..n {
            let x = (i as f64 / (n - 1) as f64 - 0.5) * PREVIEW_SPAN;
            let z = (j as f64 / (n - 1) as f64 - 0.5) * PREVIEW_SPAN;
            let s = graph.field_at(sim, node, x, z);
            let s = if s.is_finite() { s } else { 0.0 };
            vals[j * n + i] = s;
            lo = lo.min(s);
            hi = hi.max(s);
        }
    }
    let range = (hi - lo).max(1e-9);
    let mut px = vec![0u8; n * n * 4];
    for (idx, &s) in vals.iter().enumerate() {
        let g = (((s - lo) / range) * 255.0).round().clamp(0.0, 255.0) as u8;
        px[idx * 4] = g;
        px[idx * 4 + 1] = g;
        px[idx * 4 + 2] = g;
        px[idx * 4 + 3] = 0xff;
    }
    px
}
