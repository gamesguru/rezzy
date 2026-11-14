//! The key bundle: the payload sealed inside a `REZKEY` container.
//!
//! One bundle holds every Ed25519 signing key of a single origin: exactly one
//! *active* key and any number of *retired* ones. Retired keys stay so a
//! homeserver can keep publishing `old_verify_keys` and keep verifying old
//! events. The bundle is bound to its origin so a key cannot be used while
//! signing as a different server.
//!
//! Layout, all integers big-endian, strings length-prefixed UTF-8:
//!
//! ```text
//! "RZKB\0"                 magic (5 bytes; the NUL keeps it out of text forms)
//! payload_version  u16     = 1; unknown versions fail closed
//! origin           u16 len + bytes
//! key_count        u16     1..=MAX_KEYS
//! repeat key_count times:
//!   key_id         u16 len + bytes   "ed25519:<version>"
//!   algorithm      u8      = 1 (Ed25519)
//!   seed           [32]
//!   public         [32]    checked against the seed on load
//!   created_ms     u64     0 when unknown (an import)
//!   flags          u8      bit 0 active, bit 1 retired; exactly one set
//!   retired_ms     u64     0 when active, nonzero when retired
//! ```
//!
//! Times are milliseconds. Matrix `old_verify_keys.expired_ts` is also
//! milliseconds on the wire, so no unit conversion happens here.

use ed25519_zebra::{SigningKey, VerificationKey};

use crate::container;

const MAGIC: &[u8; 5] = b"RZKB\0";
const PAYLOAD_VERSION: u16 = 1;
const ALG_ED25519: u8 = 1;
const FLAG_ACTIVE: u8 = 1;
const FLAG_RETIRED: u8 = 2;
const KEY_ID_PREFIX: &str = "ed25519:";
/// Most keys one bundle may hold; a hostile file cannot force a large parse.
pub const MAX_KEYS: usize = 64;
/// Longest origin or key id, in bytes.
pub const MAX_NAME_LEN: usize = 255;

/// One signing key of a bundle.
#[derive(Clone)]
pub struct KeyEntry {
    key_id: String,
    signing_key: SigningKey,
    created_ms: u64,
    retired_ms: Option<u64>,
}

impl std::fmt::Debug for KeyEntry {
    /// Never prints the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyEntry")
            .field("key_id", &self.key_id)
            .field("created_ms", &self.created_ms)
            .field("retired_ms", &self.retired_ms)
            .finish_non_exhaustive()
    }
}

impl KeyEntry {
    /// An active key. `created_ms` is `0` when unknown.
    #[must_use]
    pub fn active(key_id: impl Into<String>, seed: [u8; 32], created_ms: u64) -> Self {
        Self {
            key_id: key_id.into(),
            signing_key: SigningKey::from(seed),
            created_ms,
            retired_ms: None,
        }
    }

    /// A retired key; `retired_ms` must be nonzero for the bundle to validate.
    #[must_use]
    pub fn retired(
        key_id: impl Into<String>,
        seed: [u8; 32],
        created_ms: u64,
        retired_ms: u64,
    ) -> Self {
        Self {
            key_id: key_id.into(),
            signing_key: SigningKey::from(seed),
            created_ms,
            retired_ms: Some(retired_ms),
        }
    }

    /// The key identifier, for example `ed25519:abc`.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// When the key was created, in milliseconds; `0` when unknown.
    #[must_use]
    pub const fn created_ms(&self) -> u64 {
        self.created_ms
    }

    /// When the key was retired, in milliseconds; `None` for the active key.
    #[must_use]
    pub const fn retired_ms(&self) -> Option<u64> {
        self.retired_ms
    }

    /// Whether this is the active key.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.retired_ms.is_none()
    }

    /// The public verification key bytes.
    #[must_use]
    pub fn public(&self) -> [u8; 32] {
        <[u8; 32]>::from(VerificationKey::from(&self.signing_key))
    }

    /// The signing key, for callers that must build their own key type.
    ///
    /// Prefer [`KeyBundle::sign`], which keeps the secret inside this crate.
    #[must_use]
    pub const fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

/// All signing keys of one origin.
#[derive(Debug, Clone)]
pub struct KeyBundle {
    origin: String,
    keys: Vec<KeyEntry>,
}

fn check_name(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_NAME_LEN {
        return Err(format!("{what} must be 1 to {MAX_NAME_LEN} bytes"));
    }
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(format!("{what} contains a control character"));
    }
    Ok(())
}

impl KeyBundle {
    /// Builds a bundle, checking every invariant the format requires.
    ///
    /// # Errors
    /// Returns an error for an empty or oversized origin, no keys or too many,
    /// a key id that is not `ed25519:<version>`, duplicate key ids, a retired
    /// key with `retired_ms == 0`, or anything but exactly one active key.
    pub fn new(origin: impl Into<String>, keys: Vec<KeyEntry>) -> Result<Self, String> {
        let origin = origin.into();
        check_name("origin", &origin)?;
        if keys.is_empty() || keys.len() > MAX_KEYS {
            return Err(format!("a bundle holds 1 to {MAX_KEYS} keys"));
        }
        let mut active = 0_usize;
        for (index, key) in keys.iter().enumerate() {
            check_name("key id", &key.key_id)?;
            if key.key_id.len() <= KEY_ID_PREFIX.len() || !key.key_id.starts_with(KEY_ID_PREFIX) {
                return Err(format!("key id {:?} is not ed25519:<version>", key.key_id));
            }
            if keys[..index].iter().any(|other| other.key_id == key.key_id) {
                return Err(format!("duplicate key id {:?}", key.key_id));
            }
            match key.retired_ms {
                None => active = active.saturating_add(1),
                Some(0) => return Err(format!("retired key {:?} has no retired time", key.key_id)),
                Some(_) => {}
            }
        }
        if active != 1 {
            return Err(format!(
                "a bundle needs exactly one active key, found {active}"
            ));
        }
        Ok(Self { origin, keys })
    }

    /// The origin this bundle signs for.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Every key, active and retired, in stored order.
    #[must_use]
    pub fn keys(&self) -> &[KeyEntry] {
        &self.keys
    }

    /// The one active key.
    ///
    /// # Panics
    /// Never for a bundle built by [`KeyBundle::new`] or [`KeyBundle::decode`],
    /// which both require exactly one active key.
    #[must_use]
    pub fn active(&self) -> &KeyEntry {
        self.keys
            .iter()
            .find(|key| key.is_active())
            .expect("a validated bundle has one active key")
    }

    /// `(key_id, public key)` for every key, in stored order.
    pub fn public_keys(&self) -> impl Iterator<Item = (&str, [u8; 32])> {
        self.keys.iter().map(|key| (key.key_id(), key.public()))
    }

    /// Signs `message` with the active key. The secret stays in this crate.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        <[u8; 64]>::from(self.active().signing_key.sign(message))
    }

    /// Whether decrypted bytes are a bundle rather than a plaintext key line.
    ///
    /// The magic contains a NUL, which no text key form can.
    #[must_use]
    pub fn is_bundle(plaintext: &[u8]) -> bool {
        plaintext.starts_with(MAGIC)
    }

    /// The binary payload. It contains the private seeds: handle as a secret.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&PAYLOAD_VERSION.to_be_bytes());
        put_str(&mut out, &self.origin);
        // `new` and `decode` cap the count at MAX_KEYS, far below u16::MAX.
        out.extend_from_slice(
            &u16::try_from(self.keys.len())
                .unwrap_or(u16::MAX)
                .to_be_bytes(),
        );
        for key in &self.keys {
            put_str(&mut out, &key.key_id);
            out.push(ALG_ED25519);
            out.extend_from_slice(&<[u8; 32]>::from(key.signing_key));
            out.extend_from_slice(&key.public());
            out.extend_from_slice(&key.created_ms.to_be_bytes());
            out.push(if key.is_active() {
                FLAG_ACTIVE
            } else {
                FLAG_RETIRED
            });
            out.extend_from_slice(&key.retired_ms.unwrap_or(0).to_be_bytes());
        }
        out
    }

    /// Parses a payload, failing closed on anything unexpected.
    ///
    /// # Errors
    /// Returns an error for a wrong magic or version, truncation, trailing
    /// bytes, an unknown algorithm or flag, a public key that does not match
    /// its seed, or any invariant [`KeyBundle::new`] enforces.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut reader = Reader(bytes);
        if reader.take(MAGIC.len())? != MAGIC {
            return Err("not a key bundle".to_owned());
        }
        let version = reader.u16()?;
        if version != PAYLOAD_VERSION {
            return Err(format!("unsupported key bundle version {version}"));
        }
        let origin = reader.string()?;
        let count = usize::from(reader.u16()?);
        if count == 0 || count > MAX_KEYS {
            return Err(format!("a bundle holds 1 to {MAX_KEYS} keys"));
        }
        let mut keys = Vec::with_capacity(count);
        for _ in 0..count {
            let key_id = reader.string()?;
            let algorithm = reader.u8()?;
            if algorithm != ALG_ED25519 {
                return Err(format!("unsupported key algorithm {algorithm}"));
            }
            let seed: [u8; 32] = reader.array()?;
            let public: [u8; 32] = reader.array()?;
            let created_ms = reader.u64()?;
            let flags = reader.u8()?;
            let retired_ms = reader.u64()?;
            let signing_key = SigningKey::from(seed);
            if public != <[u8; 32]>::from(VerificationKey::from(&signing_key)) {
                return Err(format!("public key of {key_id:?} does not match its seed"));
            }
            let retired_ms = match flags {
                FLAG_ACTIVE if retired_ms == 0 => None,
                FLAG_RETIRED if retired_ms != 0 => Some(retired_ms),
                _ => return Err(format!("key {key_id:?} has inconsistent flags")),
            };
            keys.push(KeyEntry {
                key_id,
                signing_key,
                created_ms,
                retired_ms,
            });
        }
        if !reader.0.is_empty() {
            return Err("trailing bytes after the key bundle".to_owned());
        }
        Self::new(origin, keys)
    }

    /// Encrypts the bundle under `passphrase` with production KDF costs.
    ///
    /// # Errors
    /// Returns an error if the RNG, KDF, or cipher fails.
    pub fn seal(&self, passphrase: &[u8]) -> Result<Vec<u8>, String> {
        let mut plain = self.encode();
        let sealed = container::seal(&plain, passphrase);
        plain.fill(0);
        sealed
    }

    /// Decrypts and parses a bundle file.
    ///
    /// When `expected_origin` is given, a bundle for any other origin is
    /// refused, so a key cannot be used to sign as the wrong server.
    ///
    /// # Errors
    /// Returns an error for a wrong passphrase, a corrupt or hostile file, a
    /// payload that is not a valid bundle, or an origin mismatch.
    pub fn unlock(
        file: &[u8],
        passphrase: &[u8],
        expected_origin: Option<&str>,
    ) -> Result<Self, String> {
        let mut plain = container::open(file, passphrase)?;
        let bundle = Self::decode(&plain);
        plain.fill(0);
        let bundle = bundle?;
        if let Some(expected) = expected_origin {
            if bundle.origin != expected {
                return Err(format!(
                    "key bundle is for {:?}, not {expected:?}",
                    bundle.origin
                ));
            }
        }
        Ok(bundle)
    }
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    // `check_name` caps every string at MAX_NAME_LEN, far below u16::MAX.
    out.extend_from_slice(&u16::try_from(value.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.0.len() < len {
            return Err("truncated key bundle".to_owned());
        }
        let (head, tail) = self.0.split_at(len);
        self.0 = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "truncated key bundle".to_owned())
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn string(&mut self) -> Result<String, String> {
        let len = usize::from(self.u16()?);
        if len > MAX_NAME_LEN {
            return Err("key bundle name is too long".to_owned());
        }
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|_| "key bundle name is not UTF-8".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: (u32, u32, u32) = (8, 1, 1);
    const ORIGIN: &str = "minimat.example";

    fn seed(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn two_keys() -> KeyBundle {
        KeyBundle::new(
            ORIGIN,
            vec![
                KeyEntry::retired("ed25519:old", seed(1), 1_000, 2_000),
                KeyEntry::active("ed25519:new", seed(2), 3_000),
            ],
        )
        .unwrap()
    }

    fn sealed(bundle: &KeyBundle, passphrase: &[u8]) -> Vec<u8> {
        container::seal_with(&bundle.encode(), passphrase, FAST).unwrap()
    }

    #[test]
    fn roundtrip_keeps_every_key_and_the_active_one() {
        let bundle = two_keys();
        let back = KeyBundle::decode(&bundle.encode()).unwrap();
        assert_eq!(back.origin(), ORIGIN);
        assert_eq!(back.keys().len(), 2);
        assert_eq!(back.active().key_id(), "ed25519:new");
        let old = &back.keys()[0];
        assert_eq!((old.created_ms(), old.retired_ms()), (1_000, Some(2_000)));
        assert_eq!(back.encode(), bundle.encode(), "encoding is deterministic");
    }

    #[test]
    fn unlock_roundtrip_and_origin_binding() {
        let bundle = two_keys();
        let file = sealed(&bundle, b"pw");
        let back = KeyBundle::unlock(&file, b"pw", Some(ORIGIN)).unwrap();
        assert_eq!(back.active().public(), bundle.active().public());
        let error = KeyBundle::unlock(&file, b"pw", Some("wombatx.example")).unwrap_err();
        assert!(error.contains("not \"wombatx.example\""), "{error}");
        assert!(KeyBundle::unlock(&file, b"wrong", Some(ORIGIN)).is_err());
    }

    #[test]
    fn the_production_seal_path_round_trips() {
        let bundle = two_keys();
        let file = bundle.seal(b"pw").unwrap();
        assert!(KeyBundle::unlock(&file, b"pw", None).is_ok());
    }

    #[test]
    fn tampering_with_the_sealed_file_is_rejected() {
        let file = sealed(&two_keys(), b"pw");
        for index in [9, 25, file.len() / 2, file.len() - 1] {
            let mut tampered = file.clone();
            tampered[index] ^= 1;
            assert!(
                KeyBundle::unlock(&tampered, b"pw", None).is_err(),
                "byte {index}"
            );
        }
    }

    #[test]
    fn signing_uses_the_active_key_and_verifies_against_its_public_key() {
        let bundle = two_keys();
        let signature = ed25519_zebra::Signature::from(bundle.sign(b"event"));
        let active = VerificationKey::try_from(bundle.active().public()).unwrap();
        assert!(active.verify(&signature, b"event").is_ok());
        assert!(active.verify(&signature, b"other").is_err());
        let retired = VerificationKey::try_from(bundle.keys()[0].public()).unwrap();
        assert!(retired.verify(&signature, b"event").is_err());
    }

    #[test]
    fn public_keys_lists_retired_keys_too() {
        let bundle = two_keys();
        let ids: Vec<&str> = bundle.public_keys().map(|(id, _)| id).collect();
        assert_eq!(ids, ["ed25519:old", "ed25519:new"]);
    }

    #[test]
    fn exactly_one_active_key_is_required() {
        let none = KeyBundle::new(ORIGIN, vec![KeyEntry::retired("ed25519:a", seed(1), 0, 5)]);
        assert!(none.unwrap_err().contains("exactly one active"));
        let two = KeyBundle::new(
            ORIGIN,
            vec![
                KeyEntry::active("ed25519:a", seed(1), 0),
                KeyEntry::active("ed25519:b", seed(2), 0),
            ],
        );
        assert!(two.unwrap_err().contains("exactly one active"));
        assert!(KeyBundle::new(ORIGIN, Vec::new()).is_err());
    }

    #[test]
    fn invalid_names_and_duplicates_are_rejected() {
        let key = |id: &str| KeyEntry::active(id, seed(1), 0);
        assert!(KeyBundle::new("", vec![key("ed25519:a")]).is_err());
        assert!(KeyBundle::new(ORIGIN, vec![key("curve25519:a")]).is_err());
        assert!(KeyBundle::new(ORIGIN, vec![key("ed25519:")]).is_err());
        assert!(KeyBundle::new(ORIGIN, vec![key("ed25519:a\n")]).is_err());
        let duplicate = KeyBundle::new(
            ORIGIN,
            vec![
                KeyEntry::retired("ed25519:a", seed(1), 0, 5),
                KeyEntry::active("ed25519:a", seed(2), 0),
            ],
        );
        assert!(duplicate.unwrap_err().contains("duplicate"));
        let unretired = KeyBundle::new(
            ORIGIN,
            vec![
                KeyEntry::retired("ed25519:a", seed(1), 0, 0),
                KeyEntry::active("ed25519:b", seed(2), 0),
            ],
        );
        assert!(unretired.unwrap_err().contains("no retired time"));
    }

    #[test]
    fn a_public_key_that_does_not_match_its_seed_is_rejected() {
        let mut bytes = two_keys().encode();
        // The first entry's public key sits after: magic 5, version 2,
        // origin 2+len, count 2, key id 2+len, algorithm 1, seed 32.
        let public_at = 5 + 2 + 2 + ORIGIN.len() + 2 + 2 + "ed25519:old".len() + 1 + 32;
        bytes[public_at] ^= 1;
        assert!(KeyBundle::decode(&bytes)
            .unwrap_err()
            .contains("does not match"));
    }

    #[test]
    fn unknown_versions_and_trailing_bytes_fail_closed() {
        let good = two_keys().encode();
        let mut version = good.clone();
        version[5..7].copy_from_slice(&2_u16.to_be_bytes());
        assert!(KeyBundle::decode(&version)
            .unwrap_err()
            .contains("version 2"));
        let mut trailing = good;
        trailing.push(0);
        assert!(KeyBundle::decode(&trailing)
            .unwrap_err()
            .contains("trailing"));
    }

    #[test]
    fn every_truncation_is_an_error_not_a_panic() {
        let good = two_keys().encode();
        for len in 0..good.len() {
            assert!(KeyBundle::decode(&good[..len]).is_err(), "prefix {len}");
        }
    }

    #[test]
    fn plaintext_key_lines_are_not_bundles() {
        for text in [
            &b"ed25519:abc c2VlZA\n"[..],
            b"0x31ff3051020101",
            b"RZKBnotreally",
            b"",
        ] {
            assert!(!KeyBundle::is_bundle(text));
        }
        assert!(KeyBundle::is_bundle(&two_keys().encode()));
    }

    #[test]
    fn debug_output_never_contains_the_seed() {
        let text = format!("{:?}", two_keys());
        assert!(!text.contains("signing_key"), "{text}");
        assert!(!text.contains("[2, 2"), "{text}");
    }
}
