//! Lightweight account store backing the login flow.
//!
//! NOT production-grade security: passwords are salted-hashed with the standard
//! library hasher and persisted to a local file. This mirrors the JS login flow
//! (itself an insecure stub) so the client can present a real login/signup
//! screen — it is not meant to protect real credentials.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

const ACCOUNTS_FILE: &str = "data/accounts.bin";
const SALT: u64 = 0x5015_0115_2024_0601;

/// name -> salted password hash, persisted between runs.
pub struct Accounts {
    map: Mutex<HashMap<String, u64>>,
}

fn hash_password(name: &str, password: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    SALT.hash(&mut h);
    name.hash(&mut h);
    password.hash(&mut h);
    h.finish()
}

impl Accounts {
    pub fn load() -> Self {
        let map = std::fs::read(ACCOUNTS_FILE)
            .ok()
            .and_then(|b| soils_protocol::decode::<HashMap<String, u64>>(&b))
            .unwrap_or_default();
        Self { map: Mutex::new(map) }
    }

    fn save(map: &HashMap<String, u64>) {
        if let Some(parent) = PathBuf::from(ACCOUNTS_FILE).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(ACCOUNTS_FILE, soils_protocol::encode(map));
    }

    /// Validate a login or register a new account. `Ok(())` on success,
    /// `Err(reason)` otherwise.
    pub fn authenticate(&self, name: &str, password: &str, signup: bool) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("username required".into());
        }
        let hash = hash_password(name, password);
        let mut map = self.map.lock().unwrap();
        if signup {
            match map.get(name) {
                // Re-signup with the same credentials acts as a login.
                Some(&h) if h == hash => Ok(()),
                Some(_) => Err("username already taken".into()),
                None => {
                    map.insert(name.to_string(), hash);
                    let snapshot = map.clone();
                    drop(map);
                    Self::save(&snapshot);
                    Ok(())
                }
            }
        } else {
            match map.get(name) {
                Some(&h) if h == hash => Ok(()),
                Some(_) => Err("wrong password".into()),
                None => Err("no such account — sign up first".into()),
            }
        }
    }
}
