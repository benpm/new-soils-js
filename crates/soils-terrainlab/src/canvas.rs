//! Hand-rolled node canvas on top of `egui::Scene` (pan/zoom for free, and it
//! composites correctly under `bevy_egui 0.39`, unlike egui-snarl). Draws each
//! node as a framed group, wires as bezier-ish lines behind the nodes, and
//! handles dragging, pin-to-pin wiring, and deletion.

use egui::{Color32, Pos2, Rect, Sense, Shape, Stroke, Ui, UiBuilder, Vec2, epaint::PathShape};
use soils_worldgen::graph::Axis;

use crate::graph_model::EditorGraph;
use crate::node::{EditorNode, OutChannel};

const NODE_W: f32 = 176.0;
const HEADER_H: f32 = 22.0;
const PIN_R: f32 = 5.0;
const FIELD_COLOR: Color32 = Color32::from_rgb(0x6c, 0xcf, 0x70);
const SINK_COLOR: Color32 = Color32::from_rgb(0xff, 0xa8, 0x30);
const WIRE_COLOR: Color32 = Color32::from_rgb(0x9a, 0xd0, 0xff);

/// Canvas view + interaction state that must persist between frames.
#[derive(Clone)]
pub struct CanvasState {
    pub scene_rect: Rect,
    /// An output pin the user picked, awaiting an input pin to complete a wire.
    pub pending_from: Option<usize>,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            scene_rect: Rect::from_min_size(Pos2::new(-100.0, -200.0), Vec2::new(1400.0, 900.0)),
            pending_from: None,
        }
    }
}

/// Draw the whole node graph and apply user edits to `graph` in place.
pub fn show(ui: &mut Ui, graph: &mut EditorGraph, state: &mut CanvasState) {
    let mut scene_rect = state.scene_rect;
    let resp = egui::Scene::new().zoom_range(0.15..=2.5).show(ui, &mut scene_rect, |ui| {
        // Reserve a slot so wires paint *behind* the node frames.
        let wire_slot = ui.painter().add(Shape::Noop);

        let n = graph.nodes.len();
        let mut in_pins: Vec<Vec<Pos2>> = vec![Vec::new(); n];
        let mut out_pins: Vec<Option<Pos2>> = vec![None; n];
        let mut to_remove: Option<usize> = None;

        // --- draw nodes, record pin positions ---
        for i in 0..n {
            draw_node(ui, graph, i, &mut in_pins[i], &mut out_pins[i], &mut to_remove, state);
        }

        // --- wires behind nodes ---
        let mut wire_shapes: Vec<Shape> = Vec::new();
        for w in &graph.wires {
            let (Some(from), Some(Some(to))) = (
                out_pins.get(w.from).copied().flatten(),
                in_pins.get(w.to).map(|v| v.get(w.input).copied()),
            ) else {
                continue;
            };
            wire_shapes.push(wire_shape(from, to));
        }
        // Rubber-band wire while the user is mid-connection.
        if let Some(src) = state.pending_from {
            if let Some(Some(from)) = out_pins.get(src).copied() {
                if let Some(cursor) = ui.ctx().pointer_latest_pos() {
                    // cursor is in screen space; map into scene space.
                    let cursor = ui.ctx().layer_transform_from_global(ui.layer_id())
                        .map_or(cursor, |t| t * cursor);
                    wire_shapes.push(wire_shape(from, cursor));
                }
            }
        }
        ui.painter().set(wire_slot, Shape::Vec(wire_shapes));

        if let Some(idx) = to_remove {
            graph.remove(idx);
            state.pending_from = None;
        }

        // Click on empty canvas cancels a pending connection.
        if ui.response().clicked() {
            state.pending_from = None;
        }
    });

    state.scene_rect = scene_rect;
    // Right-click empty canvas → add-node menu.
    resp.response.context_menu(|ui| {
        add_node_menu(ui, graph, state, scene_rect.center());
    });
}

/// Draw one node frame with its params and pins.
fn draw_node(
    ui: &mut Ui,
    graph: &mut EditorGraph,
    i: usize,
    in_pins: &mut Vec<Pos2>,
    out_pin: &mut Option<Pos2>,
    to_remove: &mut Option<usize>,
    state: &mut CanvasState,
) {
    let pos = graph.nodes[i].pos;
    let builder = UiBuilder::new().max_rect(Rect::from_min_size(pos, Vec2::new(NODE_W, 0.0)));
    let is_output = matches!(graph.nodes[i].kind, EditorNode::Output { .. });

    let inner = ui.scope_builder(builder, |ui| {
        ui.set_width(NODE_W);
        let frame = egui::Frame::window(ui.style());
        frame
            .show(ui, |ui| {
                ui.set_width(NODE_W);
                // Header: draggable title + delete.
                let title = graph.nodes[i].kind.title();
                ui.horizontal(|ui| {
                    let h = ui.label(egui::RichText::new(title).strong());
                    let drag = ui.interact(h.rect, egui::Id::new(("hdr", i)), Sense::drag());
                    if drag.dragged() {
                        graph.nodes[i].pos += drag.drag_delta();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            *to_remove = Some(i);
                        }
                    });
                });
                params_ui(&mut graph.nodes[i].kind, ui);
            })
            .response
    });

    let rect = inner.response.rect;

    // Input pins on the left edge.
    let inputs = graph.nodes[i].kind.input_count();
    for k in 0..inputs {
        let y = pin_y(rect, k, inputs);
        let p = Pos2::new(rect.left(), y);
        in_pins.push(p);
        let color = if is_output { SINK_COLOR } else { FIELD_COLOR };
        pin(ui, p, color);
        let r = ui.interact(
            Rect::from_center_size(p, Vec2::splat(PIN_R * 3.0)),
            egui::Id::new(("in", i, k)),
            Sense::click(),
        );
        if r.clicked() {
            if let Some(from) = state.pending_from.take() {
                graph.connect(from, i, k);
            }
        }
        if r.secondary_clicked() {
            graph.wires.retain(|w| !(w.to == i && w.input == k));
        }
        // Label the pin (e.g. a/b/t) just inside the node.
        let label = graph.nodes[i].kind.input_label(k);
        ui.painter().text(
            p + Vec2::new(9.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
    }

    // Output pin on the right edge (value nodes only).
    if graph.nodes[i].kind.output_count() == 1 {
        let p = Pos2::new(rect.right(), rect.center().y);
        *out_pin = Some(p);
        pin(ui, p, FIELD_COLOR);
        let r = ui.interact(
            Rect::from_center_size(p, Vec2::splat(PIN_R * 3.0)),
            egui::Id::new(("out", i)),
            Sense::click(),
        );
        if r.clicked() {
            state.pending_from = Some(i);
        }
    }
}

fn pin_y(rect: Rect, k: usize, count: usize) -> f32 {
    let top = rect.top() + HEADER_H + 6.0;
    let bot = rect.bottom() - 6.0;
    if count <= 1 {
        (top + bot) * 0.5
    } else {
        top + (bot - top) * (k as f32 + 0.5) / count as f32
    }
}

fn pin(ui: &Ui, p: Pos2, color: Color32) {
    ui.painter().circle(p, PIN_R, color, Stroke::new(1.0, Color32::from_gray(20)));
}

/// A gentle S-curve wire from an output pin to an input pin.
fn wire_shape(from: Pos2, to: Pos2) -> Shape {
    let dx = (to.x - from.x).abs().max(40.0) * 0.5;
    let c1 = from + Vec2::new(dx, 0.0);
    let c2 = to - Vec2::new(dx, 0.0);
    // Sample the cubic bezier into a polyline (egui CubicBezier is also fine,
    // but a polyline keeps stroking simple across zoom).
    let pts: Vec<Pos2> = (0..=16)
        .map(|s| {
            let t = s as f32 / 16.0;
            cubic(from, c1, c2, to, t)
        })
        .collect();
    Shape::Path(PathShape::line(pts, Stroke::new(2.0, WIRE_COLOR)))
}

fn cubic(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, t: f32) -> Pos2 {
    let u = 1.0 - t;
    let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
    Pos2::new(
        w[0] * p0.x + w[1] * p1.x + w[2] * p2.x + w[3] * p3.x,
        w[0] * p0.y + w[1] * p1.y + w[2] * p2.y + w[3] * p3.y,
    )
}

/// The add-node menu, shared by the canvas context menu and the top bar.
pub fn add_node_menu(ui: &mut Ui, graph: &mut EditorGraph, state: &mut CanvasState, at: Pos2) {
    ui.label("Add node");
    for proto in EditorNode::palette() {
        if ui.button(proto.title()).clicked() {
            graph.add(proto, at);
            state.pending_from = None;
            ui.close();
        }
    }
}

/// Editable parameters for a node.
fn params_ui(node: &mut EditorNode, ui: &mut Ui) {
    fn drag(ui: &mut Ui, label: &str, v: &mut f32, speed: f32) {
        ui.horizontal(|ui| {
            ui.label(label);
            ui.add(egui::DragValue::new(v).speed(speed));
        });
    }
    match node {
        EditorNode::Constant { value } => drag(ui, "value", value, 0.01),
        EditorNode::Coord { axis } => {
            egui::ComboBox::from_id_salt("axis")
                .selected_text(if *axis == Axis::X { "X" } else { "Z" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(axis, Axis::X, "X");
                    ui.selectable_value(axis, Axis::Z, "Z");
                });
        }
        EditorNode::Simplex2 { frequency, offset } => {
            drag(ui, "freq", frequency, 0.0005);
            drag(ui, "off x", &mut offset[0], 1.0);
            drag(ui, "off z", &mut offset[1], 1.0);
        }
        EditorNode::Fbm { octaves, base_frequency, lacunarity, persistence, .. } => {
            ui.horizontal(|ui| {
                ui.label("octaves");
                ui.add(egui::DragValue::new(octaves).range(1..=10));
            });
            drag(ui, "base freq", base_frequency, 0.0005);
            drag(ui, "lacunarity", lacunarity, 0.01);
            drag(ui, "persistence", persistence, 0.01);
        }
        EditorNode::RadialFalloff { center, radius, exponent } => {
            drag(ui, "cx", &mut center[0], 1.0);
            drag(ui, "cz", &mut center[1], 1.0);
            drag(ui, "radius", radius, 1.0);
            drag(ui, "exponent", exponent, 0.05);
        }
        EditorNode::ScaleBias { scale, bias } => {
            drag(ui, "scale", scale, 0.05);
            drag(ui, "bias", bias, 0.5);
        }
        EditorNode::Clamp { min, max } => {
            drag(ui, "min", min, 0.05);
            drag(ui, "max", max, 0.05);
        }
        EditorNode::Power { exponent } => drag(ui, "exponent", exponent, 0.05),
        EditorNode::Terrace { steps } => drag(ui, "steps", steps, 0.5),
        EditorNode::DomainWarp { amount } => drag(ui, "amount", amount, 1.0),
        EditorNode::Output { channel } => {
            egui::ComboBox::from_id_salt("outch")
                .selected_text(channel.label())
                .show_ui(ui, |ui| {
                    for ch in OutChannel::ALL {
                        ui.selectable_value(channel, ch, ch.label());
                    }
                });
        }
        EditorNode::Abs
        | EditorNode::Add
        | EditorNode::Sub
        | EditorNode::Mul
        | EditorNode::Min
        | EditorNode::Max
        | EditorNode::Lerp => {
            ui.label(egui::RichText::new("f(inputs)").weak());
        }
    }
}
