//! Delta-snapshot codec (plan-game-systems §4): quantized entity state,
//! encoded per entity against a per-client baseline the receiver has acked.
//!
//! Wire shape (inside `ServerMsg::Snapshot { tick, last_input_seq, payload }`):
//!
//! ```text
//! u8 flags                      (bit0: body is LZ4 compress_prepend_size)
//! body:
//!   varint entity_count
//!   per entity, NetIds ascending:
//!     varint netid_delta        (id - previous id; first is the id itself)
//!     u8 mask                   (FULL | POS | VEL | YAW)
//!     [POS]  zigzag varint dpos per axis (fixed-point 1/256 voxel, delta
//!            against the receiver's baseline; against zero when FULL)
//!     [VEL]  3 × i16 LE         (absolute, fixed-point 1/256)
//!     [YAW]  u16 LE             (absolute turn fraction)
//! ```
//!
//! Quantized integers are the single source of truth on both ends — deltas
//! are integer subtractions, so requantization drift is impossible. Velocity
//! is 1/256 fixed-point rather than the plan's f16 (same 2 bytes/axis, no
//! dependency, exact round-trips at sim speeds). `decode` is the attack
//! surface: every read is bounds-checked and entity counts are capped.

use std::collections::HashMap;

use crate::messages::EntityState;

/// Fixed-point scale for positions and velocities.
pub const QUANT: f32 = 256.0;
/// Snapshot bodies larger than this are LZ4-compressed (spawn bursts).
const COMPRESS_OVER: usize = 200;
/// Sanity cap on entities per snapshot (decode allocation guard).
const MAX_ENTITIES: usize = 4096;

const MASK_FULL: u8 = 1 << 0;
const MASK_POS: u8 = 1 << 1;
const MASK_VEL: u8 = 1 << 2;
const MASK_YAW: u8 = 1 << 3;

/// One entity's quantized replicated state.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct QuantState {
    pub pos: [i32; 3],
    pub vel: [i16; 3],
    pub yaw: u16,
}

impl QuantState {
    pub fn quantize(pos: [f32; 3], vel: [f32; 3], yaw: u16) -> Self {
        let q = |v: f32| (v * QUANT).round() as i32;
        let qv = |v: f32| ((v * QUANT).round() as i32).clamp(i16::MIN as i32, i16::MAX as i32)
            as i16;
        Self {
            pos: [q(pos[0]), q(pos[1]), q(pos[2])],
            vel: [qv(vel[0]), qv(vel[1]), qv(vel[2])],
            yaw,
        }
    }

    pub fn dequantize(&self, id: u32) -> EntityState {
        EntityState {
            id,
            pos: [
                self.pos[0] as f32 / QUANT,
                self.pos[1] as f32 / QUANT,
                self.pos[2] as f32 / QUANT,
            ],
            velocity: [
                self.vel[0] as f32 / QUANT,
                self.vel[1] as f32 / QUANT,
                self.vel[2] as f32 / QUANT,
            ],
            yaw: self.yaw,
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn get_varint(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *data.get(*at)?;
        *at += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Encode one snapshot. `entities` must be sorted by id ascending;
/// `baseline(id)` returns the receiver's acked state for `id` (None → FULL
/// record). Velocity/yaw are included only when changed vs the baseline.
pub fn encode_snapshot(
    entities: &[(u32, QuantState)],
    mut baseline: impl FnMut(u32) -> Option<QuantState>,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(entities.len() * 12 + 4);
    put_varint(&mut body, entities.len() as u64);
    let mut prev_id = 0u64;
    for &(id, state) in entities {
        put_varint(&mut body, id as u64 - prev_id);
        prev_id = id as u64;
        let base = baseline(id);
        let (mask, dpos) = match base {
            None => (MASK_FULL | MASK_POS | MASK_VEL | MASK_YAW, state.pos),
            Some(b) => {
                let dpos = [
                    state.pos[0] - b.pos[0],
                    state.pos[1] - b.pos[1],
                    state.pos[2] - b.pos[2],
                ];
                let mut mask = 0;
                if dpos != [0, 0, 0] {
                    mask |= MASK_POS;
                }
                if state.vel != b.vel {
                    mask |= MASK_VEL;
                }
                if state.yaw != b.yaw {
                    mask |= MASK_YAW;
                }
                (mask, dpos)
            }
        };
        body.push(mask);
        if mask & MASK_POS != 0 {
            for d in dpos {
                put_varint(&mut body, zigzag(d as i64));
            }
        }
        if mask & MASK_VEL != 0 {
            for v in state.vel {
                body.extend_from_slice(&v.to_le_bytes());
            }
        }
        if mask & MASK_YAW != 0 {
            body.extend_from_slice(&state.yaw.to_le_bytes());
        }
    }
    let mut out = Vec::with_capacity(body.len() + 1);
    if body.len() > COMPRESS_OVER {
        out.push(1);
        out.extend_from_slice(&lz4_flex::compress_prepend_size(&body));
    } else {
        out.push(0);
        out.extend_from_slice(&body);
    }
    out
}

/// Receiver-side state: a short per-entity ring of `(tick, state)` as
/// decoded — mirroring the server's send history, so a delta encoded against
/// "newest send at or before the acked tick" applies against the *identical*
/// state here. Applying deltas to the latest state instead would double-count
/// everything that moved between the baseline and now (the ack always lags by
/// the RTT). Shared by the real client and the test harness so there is one
/// decode path.
#[derive(Default)]
pub struct SnapshotTracker {
    states: HashMap<u32, std::collections::VecDeque<(u32, QuantState)>>,
    /// Highest snapshot tick applied (echo as `ack_tick` in `Inputs`).
    pub latest_tick: u32,
}

/// Ring depth per entity; matches the server's send-history ring.
const TRACKER_RING: usize = 64;

impl SnapshotTracker {
    /// The most recently applied state for an entity, if any.
    pub fn latest(&self, id: u32) -> Option<QuantState> {
        self.states.get(&id).and_then(|r| r.back()).map(|(_, s)| *s)
    }

    /// Apply one snapshot payload whose deltas are encoded against
    /// `baseline_tick`; returns the updated entities (id + fresh state), or
    /// None on malformed input. Unknown entities are only accepted via FULL
    /// records; delta records without a usable baseline are skipped (they can
    /// only arise from a server bug, not from loss — the transport is ordered).
    pub fn apply(
        &mut self,
        tick: u32,
        baseline_tick: u32,
        payload: &[u8],
    ) -> Option<Vec<EntityState>> {
        let flags = *payload.first()?;
        let rest = payload.get(1..)?;
        let body: Vec<u8> = if flags & 1 != 0 {
            let prefix = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
            if prefix > MAX_ENTITIES * 32 {
                return None;
            }
            lz4_flex::decompress_size_prepended(rest).ok()?
        } else {
            rest.to_vec()
        };

        let mut at = 0usize;
        let count = get_varint(&body, &mut at)? as usize;
        if count > MAX_ENTITIES {
            return None;
        }
        let mut out = Vec::with_capacity(count);
        let mut id = 0u64;
        for _ in 0..count {
            id += get_varint(&body, &mut at)?;
            let id32 = u32::try_from(id).ok()?;
            let mask = *body.get(at)?;
            at += 1;
            let mut dpos = [0i32; 3];
            if mask & MASK_POS != 0 {
                for d in &mut dpos {
                    *d = i32::try_from(unzigzag(get_varint(&body, &mut at)?)).ok()?;
                }
            }
            let mut vel = None;
            if mask & MASK_VEL != 0 {
                let mut v = [0i16; 3];
                for slot in &mut v {
                    *slot = i16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?);
                    at += 2;
                }
                vel = Some(v);
            }
            let mut yaw = None;
            if mask & MASK_YAW != 0 {
                yaw = Some(u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?));
                at += 2;
            }

            let state = if mask & MASK_FULL != 0 {
                QuantState { pos: dpos, vel: vel.unwrap_or_default(), yaw: yaw.unwrap_or(0) }
            } else {
                // Delta base: our newest recorded state at or before the
                // packet's baseline tick — the mirror of the server's
                // "newest send the client has acked".
                let Some(base) = self.states.get(&id32).and_then(|ring| {
                    ring.iter().rev().find(|(t, _)| *t <= baseline_tick).map(|(_, s)| *s)
                }) else {
                    continue;
                };
                QuantState {
                    pos: [
                        base.pos[0].checked_add(dpos[0])?,
                        base.pos[1].checked_add(dpos[1])?,
                        base.pos[2].checked_add(dpos[2])?,
                    ],
                    vel: vel.unwrap_or(base.vel),
                    yaw: yaw.unwrap_or(base.yaw),
                }
            };
            let ring = self.states.entry(id32).or_default();
            ring.push_back((tick, state));
            if ring.len() > TRACKER_RING {
                ring.pop_front();
            }
            out.push(state.dequantize(id32));
        }
        self.latest_tick = self.latest_tick.max(tick);
        Some(out)
    }

    /// Forget an entity (despawned / left interest).
    pub fn forget(&mut self, id: u32) {
        self.states.remove(&id);
    }

    /// Drop everything (warp).
    pub fn clear(&mut self) {
        self.states.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(p: [f32; 3], v: [f32; 3], yaw: u16) -> QuantState {
        QuantState::quantize(p, v, yaw)
    }

    #[test]
    fn full_then_delta_round_trip() {
        let mut tracker = SnapshotTracker::default();
        let a0 = state([282.0, 285.5, 268.25], [1.5, 0.0, -8.0], 100);
        let b0 = state([-10.0, 40.0, 7.0], [0.0, 0.0, 0.0], 9);

        // First snapshot: no baselines → FULL records.
        let p1 = encode_snapshot(&[(3, a0), (7, b0)], |_| None);
        let out = tracker.apply(1, 0, &p1).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(tracker.latest(3), Some(a0));
        assert_eq!(tracker.latest(7), Some(b0));
        assert_eq!(out[0].pos, [282.0, 285.5, 268.25]);
        assert_eq!(out[0].velocity, [1.5, 0.0, -8.0]);

        // Second: A moved slightly (pos-only delta vs the tick-1 baseline),
        // B unchanged (1-byte mask).
        let a1 = state([282.5, 285.5, 268.25], [1.5, 0.0, -8.0], 100);
        let base = [(3u32, a0), (7, b0)];
        let p2 = encode_snapshot(&[(3, a1), (7, b0)], |id| {
            base.iter().find(|(i, _)| *i == id).map(|(_, s)| *s)
        });
        assert!(p2.len() < 16, "still+small-move snapshot should be tiny, got {}", p2.len());
        tracker.apply(2, 1, &p2).unwrap();
        assert_eq!(tracker.latest(3), Some(a1));
        assert_eq!(tracker.latest_tick, 2);
    }

    #[test]
    fn deltas_apply_against_the_acked_baseline_not_the_latest() {
        // The RTT case: the server keeps encoding against tick 1 (the last
        // ack) while the receiver has already applied newer snapshots. If the
        // receiver applied deltas to its *latest* state instead of the
        // baseline, every unacked movement would double-count.
        let mut tracker = SnapshotTracker::default();
        let s1 = state([100.0, 0.0, 0.0], [8.0, 0.0, 0.0], 0);
        tracker.apply(1, 0, &encode_snapshot(&[(1, s1)], |_| None)).unwrap();

        let s2 = state([101.0, 0.0, 0.0], [8.0, 0.0, 0.0], 0);
        let p2 = encode_snapshot(&[(1, s2)], |_| Some(s1)); // baseline: tick 1
        tracker.apply(2, 1, &p2).unwrap();

        let s3 = state([102.0, 0.0, 0.0], [8.0, 0.0, 0.0], 0);
        let p3 = encode_snapshot(&[(1, s3)], |_| Some(s1)); // still tick 1!
        let out = tracker.apply(3, 1, &p3).unwrap();
        assert_eq!(out[0].pos, [102.0, 0.0, 0.0], "delta must rebase on tick 1, not tick 2");
        assert_eq!(tracker.latest(1), Some(s3));
    }

    #[test]
    fn moving_entity_costs_a_few_bytes() {
        // The §4 envelope: ~8 B per moving entity at walking speeds.
        let mut tracker = SnapshotTracker::default();
        let e0 = state([1000.0, 260.0, 1000.0], [8.0, 0.0, 0.0], 500);
        tracker.apply(1, 0, &encode_snapshot(&[(1, e0)], |_| None)).unwrap();
        let e1 = state([1000.125, 260.0, 1000.0], [8.0, 0.0, 0.0], 500);
        let p = encode_snapshot(&[(1, e1)], |id| tracker.latest(id));
        assert!(p.len() <= 9, "one moving entity should cost ≤ 9 bytes, got {}", p.len());
    }

    #[test]
    fn quantization_error_is_bounded() {
        let s = state([123.4567, -0.001, 9999.99], [3.14159, -27.9, 0.0049], 12345);
        let d = s.dequantize(1);
        for i in 0..3 {
            assert!((d.pos[i] - [123.4567, -0.001, 9999.99][i]).abs() <= 0.5 / QUANT * 2.0);
            assert!((d.velocity[i] - [3.14159, -27.9, 0.0049][i]).abs() <= 0.5 / QUANT * 2.0);
        }
    }

    #[test]
    fn decode_never_panics_on_malformed_input() {
        let mut tracker = SnapshotTracker::default();
        let good = encode_snapshot(&[(5, state([1.0, 2.0, 3.0], [0.0; 3], 7))], |_| None);
        for cut in 0..good.len() {
            let _ = tracker.apply(1, 0, &good[..cut]);
        }
        let mut s = 0x1234_5678_9abc_def0u64;
        for _ in 0..2000 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let mut m = good.clone();
            let i = (s >> 33) as usize % m.len();
            m[i] ^= (s >> 17) as u8 | 1;
            let _ = tracker.apply(1, 0, &m);
        }
        // Absurd declared counts must be rejected, not allocated.
        let mut huge = vec![0u8];
        put_varint(&mut huge, u32::MAX as u64);
        assert!(tracker.apply(1, 0, &huge).is_none());
    }
}
