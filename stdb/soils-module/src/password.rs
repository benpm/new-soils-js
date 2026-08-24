//! Password hashing, inside the module.
//!
//! The verifier never leaves the database. This is not a stylistic choice:
//! SpacetimeDB has exactly two table visibilities, and `public` means *any*
//! connected identity may subscribe and read every row. Row-level security
//! would be the natural tool, but as of 2.7.1 `client_visibility_filter` is
//! gated behind the `unstable` feature and its own source says the filters are
//! "currently unimplemented, and are not enforced" — declaring one would look
//! like a security boundary while being decoration.
//!
//! So `account` is private, which means no client — the game server included —
//! can read it. Hashing and verification therefore have to happen where the
//! row lives, and the only code that runs there is a reducer.
//!
//! The password itself reaches the reducer in plaintext, which is the same
//! trust the game server already holds (it receives the login) and is safe to
//! do here specifically because SpacetimeDB 2.0 stopped broadcasting reducer
//! arguments: a reducer's outcome is delivered only to the connection that
//! called it. Under 1.0 semantics this design would have published every
//! password to every client, so it is worth stating why it does not.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use spacetimedb::rand::Rng;
use spacetimedb::ReducerContext;

/// Produce a PHC string for `password`, salted from the module's RNG.
///
/// `ctx.rng()` rather than `OsRng`: a module runs in a WASM sandbox with no
/// operating system to ask, and SpacetimeDB's generator is the sanctioned
/// source — it is seeded per-transaction by the host.
pub fn hash(ctx: &ReducerContext, password: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    ctx.rng().fill(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| format!("salt: {e}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("hashing failed: {e}"))
}

/// Check `password` against a stored PHC string.
///
/// A malformed verifier fails closed. It cannot arise through the reducers —
/// they only ever store what [`hash`] produced — but "unparseable" must never
/// read as "matches".
pub fn verify(password: &str, verifier: &str) -> bool {
    PasswordHash::new(verifier)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}
