//! Shared types for the new-soils Rust port: coordinates, voxel storage, and
//! the client/server wire protocol. Deliberately free of Bevy and tokio so it
//! can be used by both the client and the headless server.

pub mod chunk_codec;
pub mod chunk_key;
pub mod coords;
pub mod discovery;
pub mod messages;
pub mod snapshot;
pub mod voxel;

pub use chunk_codec::{decode_chunk, encode_chunk, payload_is_air};
pub use chunk_key::{pack_chunk_key, unpack_chunk_key};
pub use coords::{
    CHUNK_BIT, CHUNK_CLIP, CHUNK_CUBED, CHUNK_SIZE, REGION_SIZE, chunk_of, chunk_origin, local_of,
    voxel_index,
};
pub use discovery::{DISCOVERY_PORT, PROBE_MAGIC, ServerInfo};
pub use messages::{
    ChunkInfo, ClientMsg, EntityState, GenParams, InputFrame, PROTOCOL_VERSION, ServerMsg, decode,
    encode,
};
pub use snapshot::{QuantState, SnapshotTracker, encode_snapshot};
pub use voxel::{AIR, ChunkVolume, Voxel};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip() {
        let msg = ClientMsg::Edit { seq: 3, pos: [10, -3, 42], value: 7 };
        let bytes = encode(&msg);
        let back: ClientMsg = decode(&bytes).expect("decode");
        match back {
            ClientMsg::Edit { seq, pos, value } => {
                assert_eq!(seq, 3);
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
        let msg = ServerMsg::Manifest {
            chunks: vec![
                ChunkInfo::Pristine { pos: [4, 5, 6] },
                ChunkInfo::Edited { pos: [1, 2, 3], payload: encode_chunk(&vol) },
            ],
        };
        let bytes = encode(&msg);
        let back: ServerMsg = decode(&bytes).expect("decode");
        match back {
            ServerMsg::Manifest { chunks } => {
                assert_eq!(chunks.len(), 2);
                assert_eq!(chunks[0].pos(), [4, 5, 6]);
                match &chunks[1] {
                    ChunkInfo::Edited { pos, payload } => {
                        assert_eq!(*pos, [1, 2, 3]);
                        let dec = decode_chunk(payload).expect("payload decodes");
                        assert_eq!(dec.get(1, 2, 3), 7);
                        assert!(!payload_is_air(payload));
                    }
                    _ => panic!("wrong info variant"),
                }
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
