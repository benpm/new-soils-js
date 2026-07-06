//! Terrain Lab — a node-based terrain designer.
//!
//! Wire noise/combine/modulate nodes into a graph, watch the heightmap update
//! in realtime, and save/load the result as a `*.terrain.ron` that the game's
//! `soils-worldgen` consumes directly. This binary owns the Bevy app, the egui
//! editor (a hand-rolled node canvas on `egui::Scene`; see `canvas`), and a
//! CPU-oracle 2D preview; the GPU compute preview and 3D view are layered on in
//! later modules.

mod canvas;
mod graph_model;
mod node;
mod node_preview;
mod preview3d;
mod wgsl_gen;

use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy_egui::{EguiContexts, EguiGlobalSettings, EguiPlugin, EguiPrimaryContextPass, egui};
use noise::Simplex;
use soils_worldgen::graph::TerrainGraph;

use canvas::CanvasState;
use graph_model::EditorGraph;
use node_preview::NodePreviews;
use preview3d::{PreviewInput, TerrainPreviewPlugin, ViewMode};

/// Side of the square 2D preview, in pixels.
const PREVIEW_PX: usize = 192;
/// World span (in voxels) the preview covers, centred on the origin.
const PREVIEW_SPAN: f64 = 2048.0;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { title: "Terrain Lab".into(), ..default() }),
            ..default()
        }))
        .add_plugins(EguiPlugin::default())
        .add_plugins(TerrainPreviewPlugin)
        .init_resource::<LabState>()
        .add_systems(Startup, disable_auto_egui_context)
        .add_systems(EguiPrimaryContextPass, ui)
        .add_systems(Update, selftest_screenshot)
        .run();
}

/// The 3D preview's `Camera3d` owns the primary egui context, so stop bevy_egui
/// from auto-spawning a second one.
fn disable_auto_egui_context(mut settings: ResMut<EguiGlobalSettings>) {
    settings.auto_create_primary_context = false;
}

/// Most undo steps to keep (bounds memory on a long editing session).
const UNDO_CAP: usize = 200;

/// Undo/redo over `EditorGraph` snapshots. A step is committed only on frames
/// where the pointer is idle, so a continuous node-drag or slider-drag
/// coalesces into a single undo step.
#[derive(Default)]
struct History {
    undo: Vec<EditorGraph>,
    redo: Vec<EditorGraph>,
    /// The last committed state, compared against to detect a change.
    baseline: Option<EditorGraph>,
}

impl History {
    fn commit(&mut self, current: &EditorGraph) {
        match &self.baseline {
            None => self.baseline = Some(current.clone()),
            Some(base) if base != current => {
                self.undo.push(base.clone());
                if self.undo.len() > UNDO_CAP {
                    self.undo.remove(0);
                }
                self.redo.clear();
                self.baseline = Some(current.clone());
            }
            _ => {}
        }
    }

    fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn undo(&mut self, current: &mut EditorGraph) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(current.clone());
            *current = prev;
            self.baseline = Some(current.clone());
        }
    }
    fn redo(&mut self, current: &mut EditorGraph) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(current.clone());
            *current = next;
            self.baseline = Some(current.clone());
        }
    }
}

/// Everything the editor owns between frames.
#[derive(Resource)]
struct LabState {
    edit: EditorGraph,
    history: History,
    canvas: CanvasState,
    previews: NodePreviews,
    /// The last graph that lowered cleanly (drives the preview and saves).
    graph: TerrainGraph,
    seed: u32,
    status: String,
    path: Option<std::path::PathBuf>,
    preview: Option<egui::TextureHandle>,
    /// Serialized `(seed, graph)` the preview texture was built from, so we
    /// only re-evaluate the oracle when something actually changed.
    preview_key: String,
    /// Height range from the last CPU preview, shared with the 3D view for the
    /// colour ramp and camera framing.
    hmin: f32,
    hmax: f32,
}

impl Default for LabState {
    fn default() -> Self {
        let graph = TerrainGraph::default_soils();
        Self {
            edit: EditorGraph::from_terrain_graph(&graph),
            history: History::default(),
            canvas: CanvasState::default(),
            previews: NodePreviews::default(),
            graph,
            seed: 0,
            status: "Loaded default terrain.".into(),
            path: None,
            preview: None,
            preview_key: String::new(),
            hmin: 0.0,
            hmax: 1.0,
        }
    }
}

fn ui(
    mut contexts: EguiContexts,
    mut state: ResMut<LabState>,
    mut mode: ResMut<ViewMode>,
    mut preview_input: ResMut<PreviewInput>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let ctx = ctx.clone();

    // Keyboard undo/redo, applied before lowering so the restored graph takes
    // effect this frame. Ctrl+Z = undo; Ctrl+Y or Ctrl+Shift+Z = redo.
    let (do_undo, do_redo) = ctx.input_mut(|i| {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let undo = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::Z));
        let redo = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL, Key::Y))
            || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::CTRL | Modifiers::SHIFT, Key::Z));
        (undo, redo)
    });
    if do_undo || do_redo {
        let LabState { history, edit, canvas, .. } = &mut *state;
        if do_undo {
            history.undo(edit);
        } else {
            history.redo(edit);
        }
        canvas.pending_from = None;
    }

    // Re-lower the editor graph; keep the last good one on error.
    match state.edit.to_terrain_graph() {
        Ok(g) => {
            state.graph = g;
            if state.status.starts_with('⚠') {
                state.status.clear();
            }
        }
        Err(e) => state.status = format!("⚠ {e}"),
    }

    top_bar(&ctx, &mut state, &mut mode);
    preview_panel(&ctx, &mut state);

    let pointer_idle = !ctx.is_using_pointer();

    // Per-node intermediate previews: lazily computed off-thread, keyed by node
    // content signature. Lower a fresh graph from the *current* edit so the
    // signatures and canonical indices match what the background task samples.
    let show_previews = *mode == ViewMode::Graph && state.previews.enabled;
    let mut sigs: Vec<u64> = Vec::new();
    let mut preview_map = std::collections::HashMap::new();
    if show_previews {
        if let Ok(g) = state.edit.to_terrain_graph() {
            let seed = state.seed;
            sigs = state.edit.node_signatures(seed);
            let canon = state.edit.canonical_map();
            preview_map = state.previews.update(&ctx, &g, seed, &sigs, &canon, pointer_idle);
        }
    }

    // In Graph mode the opaque node canvas fills the centre; in 3D mode we leave
    // it empty so the terrain (rendered by the Camera3d behind egui) shows.
    if *mode == ViewMode::Graph {
        let prev = if show_previews { Some(&preview_map) } else { None };
        // Transparent frame so the 3D terrain (rendered by the Camera3d) shows
        // through behind the node graph.
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(&ctx, |ui| {
            let LabState { edit, canvas, .. } = &mut *state;
            canvas::show(ui, edit, canvas, &sigs, prev);
        });
    }

    // Commit an undo snapshot once the pointer is idle, so held drags coalesce.
    if pointer_idle {
        let LabState { history, edit, .. } = &mut *state;
        history.commit(edit);
    }

    // Publish the current graph + height range to the 3D preview.
    preview_input.graph = Some(state.graph.clone());
    preview_input.hmin = state.hmin;
    preview_input.hmax = state.hmax;
}

fn top_bar(ctx: &egui::Context, state: &mut LabState, mode: &mut ViewMode) {
    egui::TopBottomPanel::top("top").show(ctx, |ui| {
        ui.horizontal(|ui| {
            if ui.button("New").clicked() {
                let graph = TerrainGraph::default_soils();
                state.edit = EditorGraph::from_terrain_graph(&graph);
                state.graph = graph;
                state.path = None;
                state.status = "Loaded default terrain.".into();
            }
            if ui.button("Open…").clicked() {
                open_dialog(state);
            }
            if ui.button("Save").clicked() {
                match state.path.clone() {
                    Some(p) => save_to(state, &p),
                    None => save_dialog(state),
                }
            }
            if ui.button("Save As…").clicked() {
                save_dialog(state);
            }
            ui.separator();
            if ui
                .add_enabled(state.history.can_undo(), egui::Button::new("Undo"))
                .on_hover_text("Ctrl+Z")
                .clicked()
            {
                let LabState { history, edit, canvas, .. } = &mut *state;
                history.undo(edit);
                canvas.pending_from = None;
            }
            if ui
                .add_enabled(state.history.can_redo(), egui::Button::new("Redo"))
                .on_hover_text("Ctrl+Y")
                .clicked()
            {
                let LabState { history, edit, canvas, .. } = &mut *state;
                history.redo(edit);
                canvas.pending_from = None;
            }
            ui.separator();
            let (label, next) = match *mode {
                ViewMode::Graph => ("View: Graph ▸ 3D", ViewMode::Terrain3d),
                ViewMode::Terrain3d => ("View: 3D ▸ Graph", ViewMode::Graph),
            };
            if ui.button(label).clicked() {
                *mode = next;
            }
            ui.separator();
            ui.menu_button("Add node ▾", |ui| {
                let at = state.canvas.scene_rect.center();
                let LabState { edit, canvas, .. } = state;
                canvas::add_node_menu(ui, edit, canvas, at);
            });
            if ui.button("Auto-layout").clicked() {
                let LabState { edit, canvas, .. } = &mut *state;
                edit.auto_layout();
                canvas.scene_rect = edit.bounds().expand(80.0);
            }
            ui.checkbox(&mut state.canvas.grid, "Grid");
            ui.checkbox(&mut state.canvas.snap, "Snap");
            ui.checkbox(&mut state.previews.enabled, "Previews");
            ui.separator();
            ui.label("seed");
            ui.add(egui::DragValue::new(&mut state.seed));
            ui.separator();
            ui.label(&state.status);
        });
    });
}

fn preview_panel(ctx: &egui::Context, state: &mut LabState) {
    egui::SidePanel::right("preview").min_width(PREVIEW_PX as f32 + 24.0).show(ctx, |ui| {
        ui.heading("Height / density");
        rebuild_preview_if_stale(ctx, state);
        if let Some(tex) = &state.preview {
            ui.image((tex.id(), egui::vec2(PREVIEW_PX as f32, PREVIEW_PX as f32)));
        }
        ui.label(format!("{} nodes · span {:.0} voxels", state.graph.nodes.len(), PREVIEW_SPAN));
        ui.separator();
        ui.label(
            "• Drag a node's title to move it.\n\
             • Click an output pin, then an input pin, to wire.\n\
             • Right-click an input pin to disconnect.\n\
             • Right-click a node's title (or hover + Delete) to remove.\n\
             • Right-click canvas or use 'Add node' to add.",
        );
    });
}

/// Recompute the 2D preview with the CPU oracle when `(seed, graph)` changes.
/// Height is colour-mapped; where a Structure output exists, cells are stippled
/// green in proportion to its density (a cheap large-scale scatter).
fn rebuild_preview_if_stale(ctx: &egui::Context, state: &mut LabState) {
    let key = format!("{}|{}", state.seed, ron::to_string(&state.graph).unwrap_or_default());
    if key == state.preview_key && state.preview.is_some() {
        return;
    }
    state.preview_key = key;

    let sim = Simplex::new(state.seed);
    let n = PREVIEW_PX;
    let mut heights = vec![0f64; n * n];
    let mut density = vec![0f64; n * n];
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for j in 0..n {
        for i in 0..n {
            let wx = (i as f64 / (n - 1) as f64 - 0.5) * PREVIEW_SPAN;
            let wz = (j as f64 / (n - 1) as f64 - 0.5) * PREVIEW_SPAN;
            let c = state.graph.eval_columns(&sim, wx, wz);
            heights[j * n + i] = c.height;
            density[j * n + i] = c.structure;
            lo = lo.min(c.height);
            hi = hi.max(c.height);
        }
    }
    let range = (hi - lo).max(1e-6);
    state.hmin = lo as f32;
    state.hmax = hi as f32;

    let mut px = vec![0u8; n * n * 4];
    for idx in 0..n * n {
        let t = ((heights[idx] - lo) / range) as f32;
        let [mut r, mut g, mut b] = height_color(t);
        let (i, j) = (idx % n, idx / n);
        let dens = density[idx].clamp(0.0, 1.0) as f32;
        if dens > 0.0 && hash01(i as i32, j as i32) < dens {
            (r, g, b) = (0x20, 0x80, 0x20);
        }
        px[idx * 4] = r;
        px[idx * 4 + 1] = g;
        px[idx * 4 + 2] = b;
        px[idx * 4 + 3] = 0xff;
    }

    let image = egui::ColorImage::from_rgba_unmultiplied([n, n], &px);
    state.preview = Some(ctx.load_texture("preview", image, egui::TextureOptions::NEAREST));
}

/// A terrain colour ramp for normalized height `t` in `[0, 1]`.
fn height_color(t: f32) -> [u8; 3] {
    let stops = [
        (0.0, [30, 60, 120]),
        (0.35, [70, 110, 170]),
        (0.4, [200, 190, 130]),
        (0.55, [70, 140, 60]),
        (0.8, [110, 100, 90]),
        (1.0, [240, 240, 245]),
    ];
    for w in stops.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t <= t1 {
            let f = ((t - t0) / (t1 - t0).max(1e-6)).clamp(0.0, 1.0);
            return [lerp_u8(c0[0], c1[0], f), lerp_u8(c0[1], c1[1], f), lerp_u8(c0[2], c1[2], f)];
        }
    }
    [240, 240, 245]
}

fn lerp_u8(a: u8, b: u8, f: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * f).round() as u8
}

/// Deterministic hash of integer cell coords to `[0, 1)`.
fn hash01(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x9e37_79b9) ^ (y as u32).wrapping_mul(0x85eb_ca77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    (h & 0x00ff_ffff) as f32 / 0x0100_0000 as f32
}

fn open_dialog(state: &mut LabState) {
    if let Some(path) = rfd::FileDialog::new().add_filter("terrain", &["ron"]).pick_file() {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| ron::from_str::<TerrainGraph>(&t).map_err(|e| e.to_string()))
        {
            Ok(graph) => match graph.validate() {
                Ok(()) => {
                    state.edit = EditorGraph::from_terrain_graph(&graph);
                    state.graph = graph;
                    state.status = format!("Loaded {}", path.display());
                    state.path = Some(path);
                }
                Err(e) => state.status = format!("⚠ invalid graph: {e}"),
            },
            Err(e) => state.status = format!("⚠ open failed: {e}"),
        }
    }
}

fn save_dialog(state: &mut LabState) {
    if let Some(path) = rfd::FileDialog::new()
        .add_filter("terrain", &["ron"])
        .set_file_name("terrain.ron")
        .save_file()
    {
        save_to(state, &path);
    }
}

fn save_to(state: &mut LabState, path: &std::path::Path) {
    let text = ron::ser::to_string_pretty(&state.graph, ron::ser::PrettyConfig::default())
        .expect("graph serializes");
    match std::fs::write(path, text) {
        Ok(()) => {
            state.status = format!("Saved {}", path.display());
            state.path = Some(path.to_path_buf());
        }
        Err(e) => state.status = format!("⚠ save failed: {e}"),
    }
}

/// When `SOILS_LAB_SHOT=<path>` is set, screenshot the editor after a couple of
/// seconds and then exit — so the tool can be validated headlessly.
fn selftest_screenshot(
    time: Res<Time>,
    mut phase: Local<u8>,
    mut commands: Commands,
    mut mode: ResMut<ViewMode>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(path) = std::env::var("SOILS_LAB_SHOT") else {
        return;
    };
    // `SOILS_LAB_VIEW=3d` screenshots the terrain view instead of the graph.
    if std::env::var("SOILS_LAB_VIEW").as_deref() == Ok("3d") {
        *mode = ViewMode::Terrain3d;
    }
    let t = time.elapsed_secs();
    if *phase == 0 && t > 2.5 {
        *phase = 1;
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(path));
        info!("LAB SELFTEST: screenshot requested");
    } else if *phase == 1 && t > 4.5 {
        *phase = 2;
        info!("LAB SELFTEST: done");
        exit.write(AppExit::Success);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use node::EditorNode;

    /// Committing after a change makes it undoable; undo then redo returns the
    /// exact same graph.
    #[test]
    fn history_undo_redo_round_trip() {
        let mut h = History::default();
        let mut g = EditorGraph::default();
        g.add(EditorNode::Constant { value: 1.0 }, egui::Pos2::ZERO);
        h.commit(&g); // seed baseline
        assert!(!h.can_undo());
        let before = g.clone();
        g.add(EditorNode::Abs, egui::Pos2::ZERO);
        h.commit(&g);
        assert!(h.can_undo());
        h.undo(&mut g);
        assert_eq!(g, before);
        assert!(h.can_redo());
        h.redo(&mut g);
        assert_eq!(g.nodes.len(), 2);
    }
}
