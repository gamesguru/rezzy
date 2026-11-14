//! Passphrase-encrypted signing-key files shared by `rezzy` and homeservers.
//!
//! Two layers, kept separate so each can change without the other:
//!
//! - [`container`]: the `REZKEY` file. It seals arbitrary bytes under a
//!   passphrase (Argon2id + XChaCha20-Poly1305) and knows nothing about keys.
//! - [`bundle`]: the payload sealed inside it, a versioned binary list of
//!   Ed25519 signing keys for one origin, with exactly one active key and any
//!   number of retired ones.
//!
//! Private keys never belong in a database that is routinely copied or
//! exported. Keep these files outside it and supply the passphrase separately.

pub mod bundle;
pub mod container;

pub use bundle::{KeyBundle, KeyEntry};
pub use container::{open, seal};
