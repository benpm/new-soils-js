//! Debug view visualisations, toggled with **F1**.
//!
//! Three views of the same thing — where the chunks are and what the renderer
//! actually drew:
//!
//! * **Chunk bounds** — a gizmo box per resident chunk, coloured by whether it
//!   holds a mesh slot or is pure air. Drawn without depth testing, so the
//!   lattice is readable from inside the terrain too (which is where a
//!   streaming bug is usually looked at from).
//! * **Wireframe overlay** (F2) — the greedy quads themselves, outlined in the
//!   terrain shader. There is no per-chunk mesh entity to hand to Bevy's
//!   `WireframePlugin`: the whole world is one `multi_draw_indirect` over the
//!   pooled quad buffer (see `world_draw.rs`), so the outline is drawn by
//!   `atlas.wgsl` from each quad's own parametric coordinates instead. That is
//!   also the more informative picture — it shows the *greedy* decomposition,
//!   not a triangle soup.
//! * **Minimap** — the same chunk bounds from above, as a top-down grid of the
//!   resident columns around the player.
//!
//! The mode is off by default. `SOILS_DEBUGVIZ=1` / `SOILS_WIREFRAME=1` start
//! it enabled, which is how the screenshots are captured (the self-test path
//! presses no keys), and the console has `debugviz on|off` / `wireframe on|off`
//! for the same reason.

use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use soils_protocol::{CHUNK_BIT, CHUNK_SIZE};

use crate::player::{Player, Streaming};
use crate::pool::{ChunkSlots, NO_MESH};

/// How far from the player chunk bounds are drawn, in chunks.
///
/// Not the whole residency set: at radius 8 that is 4913 boxes, which is both
/// a lot of gizmo lines and completely unreadable — the near lattice is what
/// says anything about where you are. The minimap covers the rest.
const BOUNDS_RADIUS: i32 = 4;

/// Minimap half-width, in chunk columns. Matches the largest load radius, so a
/// fully streamed world fills it exactly.
const MINIMAP_RADIUS: i32 = 8;
/// Side of one minimap cell, and the gap between cells, in logical pixels.
const CELL_PX: f32 = 7.0;
const CELL_GAP_PX: f32 = 1.0;

/// Chunk-bounds colours: a chunk carrying a mesh slot, and an air chunk (light
/// only, no draw). Air is deliberately dimmer — an empty sky box is context,
/// not the subject.
const BOUNDS_MESHED: Color = Color::srgba(0.35, 0.85, 1.0, 0.55);
const BOUNDS_AIR: Color = Color::srgba(0.45, 0.45, 0.55, 0.22);
/// The chunk the player is standing in.
const BOUNDS_PLAYER: Color = Color::srgb(1.0, 0.78, 0.25);

/// Is the debug view on, and is the wireframe overlay on inside it?
///
/// One resource rather than two so "debug viz mode" is a single thing to
/// toggle, query and print — the wireframe is a *view within* the mode, and
/// only takes effect while [`enabled`](Self::enabled) holds.
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugViz {
    pub enabled: bool,
    pub wireframe: bool,
}

impl DebugViz {
    /// Whether the terrain shader should outline quads this frame.
    pub fn wireframe_active(self) -> bool {
        self.enabled && self.wireframe
    }
}

/// Starting state, from the environment: `SOILS_DEBUGVIZ=1` opens the mode and
/// `SOILS_WIREFRAME=1` additionally opens the overlay (and implies the mode,
/// so one variable is enough to ask for the wireframe shot).
pub fn configured() -> DebugViz {
    let var = |k: &str| std::env::var(k).ok();
    from_env(var("SOILS_DEBUGVIZ").as_deref(), var("SOILS_WIREFRAME").as_deref())
}

/// The mapping `configured` implements, split out so it is testable: reading
/// the real environment from a test would race every other test in the
/// process.
fn from_env(debugviz: Option<&str>, wireframe: Option<&str>) -> DebugViz {
    let on = |v: Option<&str>| matches!(v, Some("1") | Some("on") | Some("true"));
    let wireframe = on(wireframe);
    DebugViz { enabled: on(debugviz) || wireframe, wireframe }
}

/// Its own gizmo group so the bounds can be drawn *through* terrain: a
/// negative depth bias pulls them toward the camera in clip space, which the
/// shared default group cannot be given without moving the block-selection
/// box in `edit.rs` along with it.
#[derive(Default, Reflect, GizmoConfigGroup)]
pub struct DebugVizGizmos;

/// Marker for the minimap's root node (shown and hidden with the mode).
#[derive(Component)]
pub struct MinimapRoot;

/// One minimap cell, at a chunk-column offset from the player's column.
#[derive(Component, Clone, Copy)]
pub struct MinimapCell {
    pub dx: i32,
    pub dz: i32,
}

/// The line under the minimap naming the keys and the counts.
#[derive(Component)]
pub struct MinimapLegend;

/// What one chunk column looks like from above.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellState {
    /// Nothing resident in this column.
    Empty,
    /// Resident, but every chunk in it is air (a light slot, no mesh slot).
    Air,
    /// At least one chunk in the column carries a mesh slot.
    Meshed,
}

/// Classify a column from its resident/meshed chunk counts.
pub fn cell_state(resident: u32, meshed: u32) -> CellState {
    match (resident, meshed) {
        (0, _) => CellState::Empty,
        (_, 0) => CellState::Air,
        _ => CellState::Meshed,
    }
}

/// Colour for a cell. `meshed` deepens the green so a thick column of solid
/// terrain reads differently from a single meshed chunk under open sky, and
/// the player's own column is always the accent colour.
pub fn cell_color(state: CellState, meshed: u32, is_player: bool) -> Color {
    if is_player {
        return BOUNDS_PLAYER;
    }
    match state {
        CellState::Empty => Color::srgba(1.0, 1.0, 1.0, 0.05),
        CellState::Air => Color::srgba(0.45, 0.5, 0.62, 0.35),
        CellState::Meshed => {
            // Saturate at eight meshed chunks: a column deeper than that is
            // solid whichever way you count it.
            let t = (meshed as f32 / 8.0).clamp(0.0, 1.0);
            Color::srgba(0.20 + 0.15 * t, 0.55 + 0.40 * t, 0.55 + 0.30 * t, 0.55 + 0.35 * t)
        }
    }
}

/// Resident and meshed chunk counts per column (x, z), from the slot table.
pub fn columns(slots: &ChunkSlots) -> HashMap<IVec2, (u32, u32)> {
    let mut out: HashMap<IVec2, (u32, u32)> = HashMap::default();
    for (pos, slot) in slots.iter() {
        let e = out.entry(IVec2::new(pos.x, pos.z)).or_insert((0, 0));
        e.0 += 1;
        if slot.mesh != NO_MESH {
            e.1 += 1;
        }
    }
    out
}

/// F1 toggles the mode; F2 toggles the wireframe overlay within it.
///
/// F2 also *opens* the mode when it is closed: a key that visibly does nothing
/// until you have pressed another one first is a bug report waiting to happen.
pub fn toggle_debug_viz(keys: Res<ButtonInput<KeyCode>>, mut viz: ResMut<DebugViz>) {
    if keys.just_pressed(KeyCode::F1) {
        viz.enabled = !viz.enabled;
    }
    if keys.just_pressed(KeyCode::F2) {
        viz.wireframe = !viz.wireframe;
        if viz.wireframe {
            viz.enabled = true;
        }
    }
}

/// Draw a box around every resident chunk near the player.
pub fn draw_chunk_bounds(
    viz: Res<DebugViz>,
    slots: Res<ChunkSlots>,
    player: Query<&Transform, With<Player>>,
    mut gizmos: Gizmos<DebugVizGizmos>,
) {
    if !viz.enabled {
        return;
    }
    let Ok(t) = player.single() else { return };
    let here = chunk_of(t.translation);
    for (&pos, slot) in slots.iter() {
        let d = (pos - here).abs();
        if d.max_element() > BOUNDS_RADIUS {
            continue;
        }
        let color = if pos == here {
            BOUNDS_PLAYER
        } else if slot.mesh != NO_MESH {
            BOUNDS_MESHED
        } else {
            BOUNDS_AIR
        };
        let side = CHUNK_SIZE as f32;
        let centre = pos.as_vec3() * side + Vec3::splat(side * 0.5);
        gizmos.cube(Transform::from_translation(centre).with_scale(Vec3::splat(side)), color);
    }
}

/// Bounds are drawn through terrain: a debug lattice you can only see when
/// nothing is in front of it is no use underground, which is where chunk
/// residency questions are usually asked.
pub fn setup_gizmo_config(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<DebugVizGizmos>();
    config.depth_bias = -1.0;
    config.line.width = 1.5;
}

/// Spawn the (hidden) minimap: a fixed grid of cells, top-right, plus the
/// legend line under it.
///
/// The grid is built once and only ever recoloured. Rebuilding 289 nodes every
/// frame would put UI layout on the hot path of a view whose whole purpose is
/// watching the frame clock.
pub fn setup_minimap(mut commands: Commands) {
    commands
        .spawn((
            MinimapRoot,
            Visibility::Hidden,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(8.0),
                right: Val::Px(8.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::FlexEnd,
                row_gap: Val::Px(4.0),
                padding: UiRect::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.03, 0.03, 0.04, 0.72)),
        ))
        .with_children(|root| {
            root.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(CELL_GAP_PX),
                ..default()
            })
            .with_children(|grid| {
                // Screen-space is world-space seen from above: +X to the
                // right, +Z down the screen. Rows are therefore z.
                for dz in -MINIMAP_RADIUS..=MINIMAP_RADIUS {
                    grid.spawn(Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: Val::Px(CELL_GAP_PX),
                        ..default()
                    })
                    .with_children(|row| {
                        for dx in -MINIMAP_RADIUS..=MINIMAP_RADIUS {
                            row.spawn((
                                MinimapCell { dx, dz },
                                Node {
                                    width: Val::Px(CELL_PX),
                                    height: Val::Px(CELL_PX),
                                    ..default()
                                },
                                BackgroundColor(cell_color(CellState::Empty, 0, false)),
                            ));
                        }
                    });
                }
            });
            // No width of its own: constrained to the grid's width the lines
            // wrap mid-number, and a readout that reflows as its values change
            // is unreadable at a glance.
            root.spawn((
                MinimapLegend,
                Text::new(""),
                TextFont { font_size: 11.0.into(), ..default() },
                TextColor(Color::srgba(1.0, 1.0, 1.0, 0.75)),
            ));
        });
}

/// Recolour the minimap from the current residency, and keep the legend and
/// the panel's visibility in step with the mode.
pub fn update_minimap(
    viz: Res<DebugViz>,
    slots: Res<ChunkSlots>,
    streaming: Res<Streaming>,
    player: Query<(&Player, &Transform)>,
    mut root: Query<&mut Visibility, With<MinimapRoot>>,
    mut cells: Query<(&MinimapCell, &mut BackgroundColor)>,
    mut legend: Query<&mut Text, With<MinimapLegend>>,
) {
    if let Ok(mut vis) = root.single_mut() {
        let want = if viz.enabled { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != want {
            *vis = want;
        }
    }
    // Hidden: nothing below it is on screen, so none of it is worth computing.
    if !viz.enabled {
        return;
    }
    let Ok((p, t)) = player.single() else { return };
    let here = chunk_of(t.translation);
    let cols = columns(&slots);

    for (cell, mut bg) in &mut cells {
        let col = IVec2::new(here.x + cell.dx, here.z + cell.dz);
        let (resident, meshed) = cols.get(&col).copied().unwrap_or((0, 0));
        let want = cell_color(cell_state(resident, meshed), meshed, cell.dx == 0 && cell.dz == 0);
        if bg.0 != want {
            bg.0 = want;
        }
    }
    // Over every resident column, not only the ones on the map: this sits
    // beside a count of the whole residency set and the two must be measuring
    // the same world.
    let meshed_total: u32 = cols.values().map(|&(_, meshed)| meshed).sum();

    if let Ok(mut text) = legend.single_mut() {
        // Kept to short lines that fit without wrapping, and to ASCII: the
        // default font has no glyph for a plus-minus sign and draws a box.
        let body = format!(
            "chunk minimap  facing {}\n\
             resident {}  meshed {}\n\
             load radius {}  map r{MINIMAP_RADIUS}\n\
             F1 viz ON   F2 wireframe {}",
            compass(p.yaw),
            slots.len(),
            meshed_total,
            streaming.load_radius,
            if viz.wireframe { "ON" } else { "OFF" },
        );
        if text.0 != body {
            text.0 = body;
        }
    }
}

/// The chunk a world position falls in.
fn chunk_of(pos: Vec3) -> IVec3 {
    let v = pos.floor().as_ivec3();
    IVec3::new(v.x >> CHUNK_BIT, v.y >> CHUNK_BIT, v.z >> CHUNK_BIT)
}

/// Yaw to a compass letter. Yaw 0 looks down -Z (north), and increases
/// counter-clockwise seen from above — the same convention `player::mouse_look`
/// writes.
pub fn compass(yaw: f32) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NW", "W", "SW", "S", "SE", "E", "NE"];
    let turns = yaw / std::f32::consts::TAU;
    let octant = (turns * 8.0).round().rem_euclid(8.0) as usize;
    NAMES[octant % 8]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<DebugViz>();
        app.init_resource::<ButtonInput<KeyCode>>();
        app.add_systems(Update, toggle_debug_viz);
        app
    }

    /// One key tap. `ButtonInput` normally retires `just_pressed` itself, but
    /// `bevy_input`'s systems are absent here — without the explicit clear the
    /// key reads as freshly pressed on the next frame and every tap toggles
    /// twice.
    fn press(app: &mut App, key: KeyCode) {
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(key);
        app.update();
        let mut input = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        input.release(key);
        input.clear();
    }

    fn viz(app: &App) -> DebugViz {
        *app.world().resource::<DebugViz>()
    }

    #[test]
    fn f1_toggles_the_mode() {
        let mut app = app();
        assert!(!viz(&app).enabled, "debug viz is off by default");
        press(&mut app, KeyCode::F1);
        assert!(viz(&app).enabled, "F1 must open the debug view");
        press(&mut app, KeyCode::F1);
        assert!(!viz(&app).enabled, "F1 must close it again");
    }

    #[test]
    fn f2_opens_the_wireframe_and_the_mode_with_it() {
        let mut app = app();
        press(&mut app, KeyCode::F2);
        let v = viz(&app);
        assert!(v.wireframe, "F2 must turn the overlay on");
        assert!(v.enabled, "F2 must open the mode rather than doing nothing visible");
        assert!(v.wireframe_active());
    }

    /// F1 closing the mode must take the overlay off screen with it, but
    /// remember it: reopening restores the view you were looking at.
    #[test]
    fn closing_the_mode_suspends_the_wireframe_without_forgetting_it() {
        let mut app = app();
        press(&mut app, KeyCode::F2);
        press(&mut app, KeyCode::F1);
        let v = viz(&app);
        assert!(!v.enabled);
        assert!(v.wireframe, "the overlay setting survives");
        assert!(!v.wireframe_active(), "but nothing is drawn while the mode is closed");
    }

    #[test]
    fn a_column_is_meshed_only_when_something_in_it_has_a_mesh_slot() {
        assert_eq!(cell_state(0, 0), CellState::Empty);
        assert_eq!(cell_state(5, 0), CellState::Air);
        assert_eq!(cell_state(5, 1), CellState::Meshed);
    }

    /// The census has to fold a column's chunks together, and must not count
    /// air chunks as meshed — the two states are the whole point of the map.
    #[test]
    fn columns_fold_chunks_by_xz() {
        let mut slots = ChunkSlots::default();
        // A column of three: two solid, one air above them.
        slots.alloc(IVec3::new(2, 0, -3), true).unwrap();
        slots.alloc(IVec3::new(2, 1, -3), true).unwrap();
        slots.alloc(IVec3::new(2, 2, -3), false).unwrap();
        // A neighbouring column of pure air.
        slots.alloc(IVec3::new(3, 0, -3), false).unwrap();

        let cols = columns(&slots);
        assert_eq!(cols.get(&IVec2::new(2, -3)).copied(), Some((3, 2)));
        assert_eq!(cols.get(&IVec2::new(3, -3)).copied(), Some((1, 0)));
        assert_eq!(cols.get(&IVec2::new(9, 9)).copied(), None);

        assert_eq!(cell_state(3, 2), CellState::Meshed);
        assert_eq!(cell_state(1, 0), CellState::Air);
    }

    /// The environment has to be able to open the view without a keypress:
    /// the self-test that takes the screenshots presses none, and asking for
    /// the wireframe alone must not leave the mode shut and the overlay
    /// invisible.
    #[test]
    fn the_environment_can_open_the_view_without_a_keypress() {
        assert_eq!(from_env(None, None), DebugViz { enabled: false, wireframe: false });
        assert_eq!(from_env(Some("1"), None), DebugViz { enabled: true, wireframe: false });
        assert_eq!(
            from_env(None, Some("1")),
            DebugViz { enabled: true, wireframe: true },
            "asking for the overlay must open the mode that shows it"
        );
        for truthy in ["1", "on", "true"] {
            assert!(from_env(Some(truthy), None).enabled, "{truthy:?} must read as on");
        }
        for falsy in ["0", "off", "", "yes"] {
            assert!(!from_env(Some(falsy), None).enabled, "{falsy:?} must not read as on");
        }
    }

    #[test]
    fn compass_letters_follow_yaw() {
        use std::f32::consts::{FRAC_PI_2, PI};
        assert_eq!(compass(0.0), "N");
        assert_eq!(compass(FRAC_PI_2), "W");
        assert_eq!(compass(PI), "S");
        assert_eq!(compass(-FRAC_PI_2), "E");
        // Wraps rather than panicking on an unnormalised yaw.
        assert_eq!(compass(2.0 * PI), "N");
    }
}
