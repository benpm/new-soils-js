//! Terrain shading constants, shared by the pooled world draw
//! (`world_draw.rs`) and the day/night systems. The per-chunk `Material`
//! implementation this module used to hold died with the pooled render core —
//! chunks draw through one world-global pipeline and bind group now.

use bevy::prelude::*;

/// Effective illuminance (lux-ish) applied to the unlit terrain so it sits in
/// the same exposure regime as the physically-bright atmosphere sky. Tuned so a
/// mid-albedo block lands around `albedo * 4` at the daytime exposure — bright
/// enough to read clearly through the atmosphere's in-scattering veil.
pub const TERRAIN_BRIGHTNESS: f32 = 45_000.0;

/// Exponential-squared fog density (per world unit). Tuned so terrain is crisp
/// up close and fades into the horizon haze near the chunk-load boundary.
pub const FOG_DENSITY: f32 = 0.0018;

/// Fog colour in the same lux regime as [`TERRAIN_BRIGHTNESS`] (scaled by the
/// view exposure in-shader), matched to the atmosphere's pale horizon haze so
/// the load boundary dissolves into the sky.
pub const FOG_COLOR: Vec3 = Vec3::new(23_000.0, 23_000.0, 24_000.0);
