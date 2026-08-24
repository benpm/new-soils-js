//! Account store backing the login flow.
//!
//! Two backends behind one call. With SpacetimeDB configured, accounts live in
//! its `account` table and this is the only writer; without it, they live in a
//! local file exactly as before, so single-player and offline LAN play never
//! need a database.
//!
//! Passwords are hashed with Argon2id and a per-account salt, and only the PHC
//! string is ever stored or transmitted. The module holds that string and never
//! verifies it: the game server is the only party a client proves anything to,
//! so verification belongs here.
//!
//! Accounts created by the old scheme (a `DefaultHasher` over a fixed salt) are
//! migrated on the next successful login — the legacy hash can confirm the
//! password once, which is the only moment a plaintext is available to rehash
//! from. Legacy entries that never log in again are left alone rather than
//! guessed at.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::Argon2;

/// Salt for the pre-Argon2 scheme. Kept solely to verify legacy accounts once,
/// so they can be migrated.
const LEGACY_SALT: u64 = 0x5015_0115_2024_0601;

/// Longest accepted account name. Matches the module's `MAX_NAME_LEN`, so a
/// name accepted locally cannot be rejected on the way to the database.
const MAX_NAME_LEN: usize = 32;

fn legacy_hash(name: &str, password: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    LEGACY_SALT.hash(&mut h);
    name.hash(&mut h);
    password.hash(&mut h);
    h.finish()
}

/// Hash a password into a PHC string with a fresh random salt.
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("hashing failed: {e}"))
}

/// Check a password against a stored PHC string.
pub fn verify_password(password: &str, verifier: &str) -> bool {
    PasswordHash::new(verifier)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

/// What a local account file holds. Legacy entries are `u64` hashes; migrated
/// and new ones are PHC strings.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum Stored {
    Legacy(u64),
    Phc(String),
}

/// The local file backend. Also the migration source when SpacetimeDB is on.
pub struct Accounts {
    map: Mutex<HashMap<String, Stored>>,
    path: PathBuf,
    /// When set, accounts are read from and written to SpacetimeDB instead.
    stdb: Mutex<Option<Arc<soils_stdb::StdbLink>>>,
}

impl Accounts {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("accounts.bin");
        let raw = std::fs::read(&path).ok();
        // Read the current format first, then fall back to the pre-migration
        // one, so an existing install keeps its accounts.
        let map = raw
            .as_deref()
            .and_then(soils_protocol::decode::<HashMap<String, Stored>>)
            .or_else(|| {
                raw.as_deref().and_then(soils_protocol::decode::<HashMap<String, u64>>).map(
                    |legacy| {
                        legacy.into_iter().map(|(k, v)| (k, Stored::Legacy(v))).collect()
                    },
                )
            })
            .unwrap_or_default();
        Self { map: Mutex::new(map), path, stdb: Mutex::new(None) }
    }

    /// Point authentication at SpacetimeDB. Until this is called (or if it
    /// never is), the local file is authoritative.
    pub fn use_stdb(&self, link: Arc<soils_stdb::StdbLink>) {
        *self.stdb.lock().unwrap() = Some(link);
    }

    fn save(path: &Path, map: &HashMap<String, Stored>) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, soils_protocol::encode(map));
    }

    fn put_local(&self, name: &str, stored: Stored) {
        let snapshot = {
            let mut map = self.map.lock().unwrap();
            map.insert(name.to_string(), stored);
            map.clone()
        };
        Self::save(&self.path, &snapshot);
    }

    /// Validate a login or register a new account.
    pub fn authenticate(&self, name: &str, password: &str, signup: bool) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("username required".into());
        }
        if name.len() > MAX_NAME_LEN {
            return Err(format!("username must be at most {MAX_NAME_LEN} bytes"));
        }
        let link = self.stdb.lock().unwrap().clone();
        match link {
            Some(link) if link.accounts_ready() => self.stdb_auth(&link, name, password, signup),
            // No database, or its cache is not warm yet. Falling back keeps
            // offline play working and keeps a slow database from locking
            // everyone out; the local file is still maintained in both modes.
            _ => self.local_auth(name, password, signup),
        }
    }

    fn stdb_auth(
        &self,
        link: &soils_stdb::StdbLink,
        name: &str,
        password: &str,
        signup: bool,
    ) -> Result<(), String> {
        if let Some(account) = link.account(name) {
            if !verify_password(password, &account.verifier) {
                return Err(if signup {
                    "username already taken".into()
                } else {
                    "wrong password".into()
                });
            }
            return Ok(());
        }

        // Not in the database yet. An account that exists locally migrates on
        // this login — the only moment a plaintext is available to rehash from.
        let local = self.map.lock().unwrap().get(name).cloned();
        match (local, signup) {
            (Some(stored), _) => {
                if !Self::check_local(name, password, &stored) {
                    return Err("wrong password".into());
                }
                let verifier = hash_password(password)?;
                link.send(soils_stdb::StdbCmd::RegisterAccount {
                    name: name.to_string(),
                    verifier: verifier.clone(),
                })
                .map_err(|e| format!("account store unavailable: {e}"))?;
                self.put_local(name, Stored::Phc(verifier));
                println!("auth: migrated account '{name}' to SpacetimeDB");
                Ok(())
            }
            (None, true) => {
                let verifier = hash_password(password)?;
                link.send(soils_stdb::StdbCmd::RegisterAccount {
                    name: name.to_string(),
                    verifier: verifier.clone(),
                })
                .map_err(|e| format!("account store unavailable: {e}"))?;
                // Kept locally too, so the server still works if the database
                // goes away later.
                self.put_local(name, Stored::Phc(verifier));
                Ok(())
            }
            (None, false) => Err("no such account — sign up first".into()),
        }
    }

    fn check_local(name: &str, password: &str, stored: &Stored) -> bool {
        match stored {
            Stored::Phc(v) => verify_password(password, v),
            Stored::Legacy(h) => legacy_hash(name, password) == *h,
        }
    }

    fn local_auth(&self, name: &str, password: &str, signup: bool) -> Result<(), String> {
        let existing = self.map.lock().unwrap().get(name).cloned();
        match existing {
            Some(stored) => {
                if !Self::check_local(name, password, &stored) {
                    return Err(if signup {
                        "username already taken".into()
                    } else {
                        "wrong password".into()
                    });
                }
                // Upgrade a legacy hash now that the password is in hand.
                if matches!(stored, Stored::Legacy(_)) {
                    self.put_local(name, Stored::Phc(hash_password(password)?));
                }
                Ok(())
            }
            None if signup => {
                self.put_local(name, Stored::Phc(hash_password(password)?));
                Ok(())
            }
            None => Err("no such account — sign up first".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts() -> (Accounts, tempdir::Dir) {
        let dir = tempdir::Dir::new("soils-auth");
        (Accounts::load(dir.path()), dir)
    }

    /// Minimal scratch directory; the crate has no dev-dependency for this.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "{tag}-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn signup_then_login() {
        let (a, _d) = accounts();
        assert!(a.authenticate("ben", "hunter2", true).is_ok());
        assert!(a.authenticate("ben", "hunter2", false).is_ok());
        assert!(a.authenticate("ben", "wrong", false).is_err());
    }

    #[test]
    fn login_before_signup_is_refused() {
        let (a, _d) = accounts();
        assert!(a.authenticate("ghost", "x", false).is_err());
    }

    #[test]
    fn signup_over_an_existing_name_needs_the_password() {
        let (a, _d) = accounts();
        a.authenticate("ben", "hunter2", true).unwrap();
        // Re-signup with the right password acts as a login, as it always has.
        assert!(a.authenticate("ben", "hunter2", true).is_ok());
        assert!(a.authenticate("ben", "guess", true).is_err());
    }

    #[test]
    fn passwords_are_not_stored_in_the_clear_or_as_a_bare_hash() {
        let (a, _d) = accounts();
        a.authenticate("ben", "hunter2", true).unwrap();
        let map = a.map.lock().unwrap();
        let Stored::Phc(v) = map.get("ben").unwrap() else { panic!("not a PHC string") };
        assert!(v.starts_with("$argon2"), "expected an argon2 PHC string, got {v}");
        assert!(!v.contains("hunter2"));
    }

    #[test]
    fn the_same_password_hashes_differently_for_two_accounts() {
        // Per-account salts: the old scheme salted with a single constant, so
        // identical passwords produced identical hashes and were visibly equal
        // to anyone holding the file.
        let (a, _d) = accounts();
        a.authenticate("ben", "same", true).unwrap();
        a.authenticate("sam", "same", true).unwrap();
        let map = a.map.lock().unwrap();
        assert_ne!(map.get("ben"), map.get("sam"));
    }

    #[test]
    fn legacy_accounts_verify_and_upgrade_on_next_login() {
        let (a, _d) = accounts();
        a.put_local("old", Stored::Legacy(legacy_hash("old", "hunter2")));
        assert!(a.authenticate("old", "wrong", false).is_err());
        assert!(a.authenticate("old", "hunter2", false).is_ok());
        let map = a.map.lock().unwrap();
        assert!(
            matches!(map.get("old"), Some(Stored::Phc(_))),
            "a legacy account should be rehashed once its password is known"
        );
    }

    #[test]
    fn accounts_survive_a_reload() {
        let dir = tempdir::Dir::new("soils-auth-reload");
        {
            let a = Accounts::load(dir.path());
            a.authenticate("ben", "hunter2", true).unwrap();
        }
        let b = Accounts::load(dir.path());
        assert!(b.authenticate("ben", "hunter2", false).is_ok());
        assert!(b.authenticate("ben", "nope", false).is_err());
    }

    #[test]
    fn a_pre_migration_file_still_loads() {
        // The on-disk format changed from HashMap<String, u64>; an existing
        // install must not lose its accounts.
        let dir = tempdir::Dir::new("soils-auth-legacyfile");
        let mut legacy: HashMap<String, u64> = HashMap::new();
        legacy.insert("old".into(), legacy_hash("old", "hunter2"));
        std::fs::write(dir.path().join("accounts.bin"), soils_protocol::encode(&legacy)).unwrap();

        let a = Accounts::load(dir.path());
        assert!(a.authenticate("old", "hunter2", false).is_ok());
    }

    #[test]
    fn names_are_bounded() {
        let (a, _d) = accounts();
        let long = "x".repeat(MAX_NAME_LEN + 1);
        assert!(a.authenticate(&long, "p", true).is_err());
        assert!(a.authenticate("", "p", true).is_err());
    }
}
