//! Data-driven entity kinds, mirroring the `blocks.yaml` pattern
//! (plan-game-systems §2): `entities.yaml` is embedded at compile time so the
//! client and server binaries always agree on kind ids (the declaration
//! index).

use serde::Deserialize;

/// Raw YAML entry.
#[derive(Debug, Clone, Deserialize)]
pub struct EntityDef {
    pub name: String,
    /// AABB half-extents around the entity center (x, y, z).
    pub half_extents: [f32; 3],
    /// Base movement speed, world units per second.
    pub speed: f32,
    /// Client render hint ("capsule", "cube", ...).
    pub render: String,
}

/// Parsed registry; kind id `u16` = declaration index.
#[derive(Debug, Clone)]
pub struct EntityRegistry {
    defs: Vec<EntityDef>,
}

impl EntityRegistry {
    pub fn parse(yaml: &str) -> Self {
        let defs: Vec<EntityDef> = serde_yaml::from_str(yaml).expect("entities.yaml parses");
        assert!(defs.len() <= u16::MAX as usize + 1, "kind ids are u16");
        Self { defs }
    }

    pub fn get(&self, kind: u16) -> Option<&EntityDef> {
        self.defs.get(kind as usize)
    }

    pub fn id_of(&self, name: &str) -> Option<u16> {
        self.defs.iter().position(|d| d.name == name).map(|i| i as u16)
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

/// The embedded registry source.
pub const ENTITIES_YAML: &str = include_str!("../entities.yaml");

/// Parse the embedded registry (both binaries call this once at startup).
pub fn default_entity_registry() -> EntityRegistry {
    EntityRegistry::parse(ENTITIES_YAML)
}

/// Well-known kind ids, asserted against the YAML in tests.
pub const KIND_PLAYER: u16 = 0;
pub const KIND_CRITTER: u16 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registry_parses_with_stable_ids() {
        let reg = default_entity_registry();
        assert_eq!(reg.id_of("Player"), Some(KIND_PLAYER));
        assert_eq!(reg.id_of("Critter"), Some(KIND_CRITTER));
        let player = reg.get(KIND_PLAYER).unwrap();
        assert_eq!(player.render, "capsule");
        assert!(player.speed > 0.0 && player.half_extents[1] > 0.0);
    }
}
