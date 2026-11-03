// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Versioned sketch profiles and session pinning.
//!
//! These types define the local protocol contract. A transport must authenticate
//! capability negotiation and bind the selected profile ID to each exchange.

use alloc::vec::Vec;

/// Current wire-format version for [`SketchHeader`].
pub const SKETCH_HEADER_VERSION: u8 = 1;
const SKETCH_HEADER_MAGIC: [u8; 4] = *b"RZSK";
const SKETCH_HEADER_PREFIX_LEN: usize = 8;

/// Immutable sketch algorithms supported by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// MSC4521's legacy GF(2^64) short-ID sketch.
    AlgebraicV1,
    /// Full-digest GF(2^256) sketch with fixed digest windows.
    AlgebraicV2Gf256,
}

impl Profile {
    /// Stable wire identifier. Changing its meaning requires a new identifier.
    #[must_use]
    pub const fn wire_id(self) -> &'static str {
        match self {
            Self::AlgebraicV1 => "algebraic_v1",
            Self::AlgebraicV2Gf256 => "algebraic_v2_gf256",
        }
    }

    const fn wire_id_len(self) -> u8 {
        match self {
            Self::AlgebraicV1 => 12,
            Self::AlgebraicV2Gf256 => 18,
        }
    }

    /// Resolves only identifiers defined by this crate.
    #[must_use]
    pub fn from_wire_id(id: &[u8]) -> Option<Self> {
        if id == Self::AlgebraicV1.wire_id().as_bytes() {
            Some(Self::AlgebraicV1)
        } else if id == Self::AlgebraicV2Gf256.wire_id().as_bytes() {
            Some(Self::AlgebraicV2Gf256)
        } else {
            None
        }
    }

    /// Maximum supported sketch capacity for this profile.
    #[must_use]
    pub const fn max_capacity(self) -> usize {
        crate::MAX_OVERFLOW_SKETCH_CAPACITY
    }

    /// Exact binary payload length for a capacity, excluding headers and text
    /// encodings.
    ///
    /// # Errors
    /// Returns [`ProfileError::InvalidCapacity`] for zero or an unsupported
    /// capacity, and [`ProfileError::LengthOverflow`] if the byte length cannot
    /// be represented by `usize`.
    pub fn payload_len(self, capacity: usize) -> Result<usize, ProfileError> {
        if capacity == 0 || capacity > self.max_capacity() {
            return Err(ProfileError::InvalidCapacity);
        }
        let bytes_per_coordinate = match self {
            Self::AlgebraicV1 => 8,
            Self::AlgebraicV2Gf256 => 32,
        };
        capacity
            .checked_mul(bytes_per_coordinate)
            .ok_or(ProfileError::LengthOverflow)
    }
}

/// Preference when selecting one profile supported by both peers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProfilePolicy {
    /// Keep v1 as the default; use v2 only if v1 is unavailable.
    #[default]
    Allow,
    /// Select v2 when common, otherwise use v1 if common.
    Prefer,
    /// Require v2; absence is an error so the caller can choose a
    /// non-algebraic path without silently downgrading.
    Require,
}

/// Failure while negotiating or validating a sketch profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileError {
    /// No profile satisfies the peer capabilities and local policy.
    NoCommonProfile,
    /// A profile ID is unknown or has invalid framing.
    InvalidHeader,
    /// The capacity is zero or exceeds the profile maximum.
    InvalidCapacity,
    /// The payload length does not match the header's profile and capacity.
    PayloadLengthMismatch,
    /// The calculated payload length overflows `usize`.
    LengthOverflow,
    /// A header uses a different profile than the pinned session.
    SessionProfileMismatch,
}

/// Selects and pins the profile for a whole reconciliation exchange.
///
/// The transport caller must authenticate capability negotiation, include the
/// selected profile in each signed request, and verify the response echo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedProfile(Profile);

impl PinnedProfile {
    /// Selects one profile from the intersection of local and peer capabilities.
    ///
    /// The selection is independent of the order in either capability list.
    ///
    /// # Errors
    /// Returns [`ProfileError::NoCommonProfile`] when no supported profile
    /// satisfies `policy`.
    pub fn negotiate(
        local: &[Profile],
        peer: &[Profile],
        policy: ProfilePolicy,
    ) -> Result<Self, ProfileError> {
        let supports = |profile| local.contains(&profile) && peer.contains(&profile);
        let selected = match policy {
            ProfilePolicy::Allow => {
                if supports(Profile::AlgebraicV1) {
                    Some(Profile::AlgebraicV1)
                } else if supports(Profile::AlgebraicV2Gf256) {
                    Some(Profile::AlgebraicV2Gf256)
                } else {
                    None
                }
            }
            ProfilePolicy::Prefer => {
                if supports(Profile::AlgebraicV2Gf256) {
                    Some(Profile::AlgebraicV2Gf256)
                } else if supports(Profile::AlgebraicV1) {
                    Some(Profile::AlgebraicV1)
                } else {
                    None
                }
            }
            ProfilePolicy::Require => {
                supports(Profile::AlgebraicV2Gf256).then_some(Profile::AlgebraicV2Gf256)
            }
        };
        selected.map(Self).ok_or(ProfileError::NoCommonProfile)
    }

    /// Profile selected for this exchange.
    #[must_use]
    pub const fn profile(self) -> Profile {
        self.0
    }

    /// Stable profile identifier that the caller must authenticate.
    #[must_use]
    pub const fn profile_id(self) -> &'static str {
        self.0.wire_id()
    }

    /// Creates a header for a sketch in the pinned session.
    ///
    /// # Errors
    /// Returns [`ProfileError::InvalidCapacity`] when `capacity` is not
    /// supported by the selected profile.
    pub fn header(self, capacity: usize) -> Result<SketchHeader, ProfileError> {
        self.0.payload_len(capacity)?;
        Ok(SketchHeader {
            profile: self.0,
            capacity: u16::try_from(capacity).map_err(|_| ProfileError::InvalidCapacity)?,
        })
    }

    /// Rejects a sketch header that attempts to switch profiles mid-session.
    ///
    /// # Errors
    /// Returns [`ProfileError::SessionProfileMismatch`] when the profile differs.
    pub fn validate_header(self, header: SketchHeader) -> Result<(), ProfileError> {
        if self.0 == header.profile {
            Ok(())
        } else {
            Err(ProfileError::SessionProfileMismatch)
        }
    }
}

/// Versioned sketch envelope metadata. The payload remains a separate byte
/// string whose length is checked against this header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SketchHeader {
    profile: Profile,
    capacity: u16,
}

impl SketchHeader {
    /// Builds and validates a header directly.
    ///
    /// # Errors
    /// Returns [`ProfileError::InvalidCapacity`] for unsupported capacities.
    pub fn new(profile: Profile, capacity: usize) -> Result<Self, ProfileError> {
        profile.payload_len(capacity)?;
        Ok(Self {
            profile,
            capacity: u16::try_from(capacity).map_err(|_| ProfileError::InvalidCapacity)?,
        })
    }

    /// Profile named by this header.
    #[must_use]
    pub const fn profile(self) -> Profile {
        self.profile
    }

    /// Sketch capacity named by this header.
    #[must_use]
    pub const fn capacity(self) -> usize {
        self.capacity as usize
    }

    /// Serializes as magic, header version, profile-ID length, big-endian
    /// capacity, then the canonical profile ID bytes.
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let id = self.profile.wire_id().as_bytes();
        let id_len = self.profile.wire_id_len();
        let capacity = self.capacity.to_be_bytes();
        let mut bytes = Vec::with_capacity(SKETCH_HEADER_PREFIX_LEN.saturating_add(id.len()));
        bytes.extend_from_slice(&SKETCH_HEADER_MAGIC);
        bytes.push(SKETCH_HEADER_VERSION);
        bytes.push(id_len);
        bytes.extend_from_slice(&capacity);
        bytes.extend_from_slice(id);
        bytes
    }

    /// Parses a complete header and rejects unknown profile IDs or invalid
    /// capacities.
    ///
    /// # Errors
    /// Returns [`ProfileError::InvalidHeader`] for malformed or unknown IDs,
    /// and [`ProfileError::InvalidCapacity`] for invalid capacities.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProfileError> {
        if bytes.get(..4) != Some(SKETCH_HEADER_MAGIC.as_slice())
            || bytes.get(4) != Some(&SKETCH_HEADER_VERSION)
        {
            return Err(ProfileError::InvalidHeader);
        }
        let id_len = usize::from(*bytes.get(5).ok_or(ProfileError::InvalidHeader)?);
        let capacity_bytes: [u8; 2] = bytes
            .get(6..8)
            .ok_or(ProfileError::InvalidHeader)?
            .try_into()
            .map_err(|_| ProfileError::InvalidHeader)?;
        let header_len = SKETCH_HEADER_PREFIX_LEN
            .checked_add(id_len)
            .ok_or(ProfileError::LengthOverflow)?;
        if bytes.len() != header_len {
            return Err(ProfileError::InvalidHeader);
        }
        let id = bytes
            .get(SKETCH_HEADER_PREFIX_LEN..header_len)
            .ok_or(ProfileError::InvalidHeader)?;
        let profile = Profile::from_wire_id(id).ok_or(ProfileError::InvalidHeader)?;
        let capacity = usize::from(u16::from_be_bytes(capacity_bytes));
        profile.payload_len(capacity)?;
        Ok(Self {
            profile,
            capacity: u16::try_from(capacity).map_err(|_| ProfileError::InvalidCapacity)?,
        })
    }

    /// Checks that a binary payload has the exact length required by the
    /// profile and capacity.
    ///
    /// # Errors
    /// Returns [`ProfileError::PayloadLengthMismatch`] when the byte length is
    /// not exact.
    pub fn validate_payload(self, payload: &[u8]) -> Result<(), ProfileError> {
        let expected = self.profile.payload_len(self.capacity())?;
        if payload.len() == expected {
            Ok(())
        } else {
            Err(ProfileError::PayloadLengthMismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILES: [Profile; 2] = [Profile::AlgebraicV1, Profile::AlgebraicV2Gf256];

    #[test]
    fn policy_selects_profile_independent_of_advertisement_order() {
        let reversed = [Profile::AlgebraicV2Gf256, Profile::AlgebraicV1];
        assert_eq!(
            PinnedProfile::negotiate(&PROFILES, &reversed, ProfilePolicy::Allow).unwrap(),
            PinnedProfile(Profile::AlgebraicV1)
        );
        assert_eq!(
            PinnedProfile::negotiate(&PROFILES, &reversed, ProfilePolicy::Prefer).unwrap(),
            PinnedProfile(Profile::AlgebraicV2Gf256)
        );
        assert_eq!(
            PinnedProfile::negotiate(&PROFILES, &[Profile::AlgebraicV1], ProfilePolicy::Require),
            Err(ProfileError::NoCommonProfile)
        );
        assert_eq!(
            PinnedProfile::negotiate(
                &PROFILES,
                &[Profile::AlgebraicV2Gf256],
                ProfilePolicy::Require
            )
            .unwrap()
            .profile(),
            Profile::AlgebraicV2Gf256
        );
    }

    #[test]
    fn header_round_trips_and_binds_profile_capacity_and_payload_length() {
        assert_eq!(
            usize::from(Profile::AlgebraicV1.wire_id_len()),
            Profile::AlgebraicV1.wire_id().len()
        );
        assert_eq!(
            usize::from(Profile::AlgebraicV2Gf256.wire_id_len()),
            Profile::AlgebraicV2Gf256.wire_id().len()
        );
        let header = SketchHeader::new(Profile::AlgebraicV2Gf256, 32).unwrap();
        assert_eq!(SketchHeader::decode(&header.encode()), Ok(header));
        assert_eq!(Profile::AlgebraicV2Gf256.payload_len(32), Ok(1024));
        assert_eq!(Profile::AlgebraicV2Gf256.payload_len(256), Ok(8192));
        assert_eq!(header.validate_payload(&[0; 1024]), Ok(()));
        assert_eq!(
            header.validate_payload(&[0; 1023]),
            Err(ProfileError::PayloadLengthMismatch)
        );
        let session = PinnedProfile(Profile::AlgebraicV1);
        assert_eq!(
            session.validate_header(header),
            Err(ProfileError::SessionProfileMismatch)
        );
    }

    #[test]
    fn decoder_rejects_unknown_or_malformed_header_ids() {
        let mut bytes = SketchHeader::new(Profile::AlgebraicV1, 8).unwrap().encode();
        let last = bytes.len().checked_sub(1).unwrap();
        bytes[last] ^= 1;
        assert_eq!(
            SketchHeader::decode(&bytes),
            Err(ProfileError::InvalidHeader)
        );
        assert_eq!(SketchHeader::decode(&[]), Err(ProfileError::InvalidHeader));
    }

    #[test]
    fn capacity_bounds_are_enforced_for_every_profile() {
        for profile in PROFILES {
            assert_eq!(profile.payload_len(0), Err(ProfileError::InvalidCapacity));
            let above_max = profile.max_capacity().checked_add(1).unwrap();
            assert_eq!(
                profile.payload_len(above_max),
                Err(ProfileError::InvalidCapacity)
            );
            assert_eq!(
                SketchHeader::new(profile, 0),
                Err(ProfileError::InvalidCapacity)
            );
            assert_eq!(
                SketchHeader::new(profile, above_max),
                Err(ProfileError::InvalidCapacity)
            );
        }
        assert_eq!(Profile::AlgebraicV1.payload_len(8), Ok(64));
        assert_eq!(Profile::AlgebraicV2Gf256.payload_len(8), Ok(256));
    }

    #[test]
    fn malformed_headers_and_payload_lengths_are_rejected() {
        let good = SketchHeader::new(Profile::AlgebraicV2Gf256, 32)
            .unwrap()
            .encode();

        let mut wrong_magic = good.clone();
        wrong_magic[0] ^= 1;
        assert_eq!(
            SketchHeader::decode(&wrong_magic),
            Err(ProfileError::InvalidHeader)
        );

        let mut wrong_version = good.clone();
        wrong_version[4] = SKETCH_HEADER_VERSION.checked_add(1).unwrap();
        assert_eq!(
            SketchHeader::decode(&wrong_version),
            Err(ProfileError::InvalidHeader)
        );

        assert_eq!(
            SketchHeader::decode(&good[..good.len() - 1]),
            Err(ProfileError::InvalidHeader)
        );

        let mut trailing = good;
        trailing.push(0);
        assert_eq!(
            SketchHeader::decode(&trailing),
            Err(ProfileError::InvalidHeader)
        );

        let mut zero_capacity = SketchHeader::new(Profile::AlgebraicV1, 8).unwrap().encode();
        zero_capacity[6..8].copy_from_slice(&0_u16.to_be_bytes());
        assert_eq!(
            SketchHeader::decode(&zero_capacity),
            Err(ProfileError::InvalidCapacity)
        );

        let header = SketchHeader::new(Profile::AlgebraicV2Gf256, 16).unwrap();
        assert_eq!(
            header.validate_payload(&[0; 1024]),
            Err(ProfileError::PayloadLengthMismatch)
        );
    }

    #[test]
    fn allow_selects_v2_only_when_v1_is_unavailable() {
        let only_v2 = [Profile::AlgebraicV2Gf256];
        assert_eq!(
            PinnedProfile::negotiate(&only_v2, &only_v2, ProfilePolicy::Allow)
                .unwrap()
                .profile(),
            Profile::AlgebraicV2Gf256
        );
        assert_eq!(
            PinnedProfile::negotiate(&PROFILES, &PROFILES, ProfilePolicy::Allow)
                .unwrap()
                .profile(),
            Profile::AlgebraicV1
        );
        assert_eq!(
            PinnedProfile::negotiate(&PROFILES, &[], ProfilePolicy::Allow),
            Err(ProfileError::NoCommonProfile)
        );
        assert_eq!(
            PinnedProfile::negotiate(
                &[Profile::AlgebraicV1],
                &[Profile::AlgebraicV1],
                ProfilePolicy::Prefer
            )
            .unwrap()
            .profile(),
            Profile::AlgebraicV1
        );
    }
}
