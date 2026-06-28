//! Shared types for the new-soils Rust port: coordinates, voxel storage, and
//! the client/server wire protocol. Deliberately free of Bevy and tokio so it
//! can be used by both the client and the headless server.

pub mod coords;
pub mod messages;
pub mod voxel;

pub use coords::{
    CHUNK_BIT, CHUNK_CLIP, CHUNK_CUBED, CHUNK_SIZE, REGION_SIZE, chunk_of, chunk_origin, local_of,
    voxel_index,
};
pub use messages::{ActorState, ChunkData, ClientMsg, ServerMsg, decode, encode};
pub use voxel::{AIR, ChunkVolume, Voxel};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip() {
        let msg = ClientMsg::Edit { pos: [10, -3, 42], value: 7 };
        let bytes = encode(&msg);
        let back: ClientMsg = decode(&bytes).expect("decode");
        match back {
            ClientMsg::Edit { pos, value } => {
                assert_eq!(pos, [10, -3, 42]);
                assert_eq!(value, 7);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn chunk_message_round_trip() {
        let msg = ServerMsg::Chunk { pos: [1, 2, 3], empty: false, voxels: vec![1, 2, 3, 0, 5] };
        let bytes = encode(&msg);
        let back: ServerMsg = decode(&bytes).expect("decode");
        match back {
            ServerMsg::Chunk { pos, empty, voxels } => {
                assert_eq!(pos, [1, 2, 3]);
                assert!(!empty);
                assert_eq!(voxels, vec![1, 2, 3, 0, 5]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn coord_conversions() {
        // Voxel (33, -1, 64) -> chunk (1, -1, 2), local (1, 31, 0).
        let v = glam::IVec3::new(33, -1, 64);
        assert_eq!(chunk_of(v), glam::IVec3::new(1, -1, 2));
        assert_eq!(local_of(v), glam::IVec3::new(1, 31, 0));
    }
}
