//! Shared types for the new-soils Rust port: coordinates, voxel storage, and
//! the client/server wire protocol. Deliberately free of Bevy and tokio so it
//! can be used by both the client and the headless server.

pub mod chunk_codec;
pub mod coords;
pub mod discovery;
pub mod messages;
pub mod voxel;

pub use chunk_codec::{decode_chunk, encode_chunk, payload_is_air};
pub use coords::{
    CHUNK_BIT, CHUNK_CLIP, CHUNK_CUBED, CHUNK_SIZE, REGION_SIZE, chunk_of, chunk_origin, local_of,
    voxel_index,
};
pub use discovery::{DISCOVERY_PORT, PROBE_MAGIC, ServerInfo};
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
        let mut vol = ChunkVolume::empty();
        vol.set(1, 2, 3, 7);
        let msg = ServerMsg::Chunk { pos: [1, 2, 3], payload: encode_chunk(&vol) };
        let bytes = encode(&msg);
        let back: ServerMsg = decode(&bytes).expect("decode");
        match back {
            ServerMsg::Chunk { pos, payload } => {
                assert_eq!(pos, [1, 2, 3]);
                let dec = decode_chunk(&payload).expect("payload decodes");
                assert_eq!(dec.get(1, 2, 3), 7);
                assert!(!payload_is_air(&payload));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn server_info_round_trip() {
        let info = ServerInfo { name: "new-soils".into(), game_port: 9001, players: 3 };
        let bytes = encode(&info);
        let back: ServerInfo = decode(&bytes).expect("decode");
        assert_eq!(back.name, "new-soils");
        assert_eq!(back.game_port, 9001);
        assert_eq!(back.players, 3);
    }

    #[test]
    fn coord_conversions() {
        // Voxel (33, -1, 64) -> chunk (1, -1, 2), local (1, 31, 0).
        let v = glam::IVec3::new(33, -1, 64);
        assert_eq!(chunk_of(v), glam::IVec3::new(1, -1, 2));
        assert_eq!(local_of(v), glam::IVec3::new(1, 31, 0));
    }
}
