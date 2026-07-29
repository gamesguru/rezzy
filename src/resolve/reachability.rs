//! Pure reachability contract for room DAG accelerators.
//!
//! This module intentionally stays free of storage, threading, and cache
//! policy. It defines only the query result type and the minimal trait that a
//! drop-in accelerator must satisfy.

/// Tri-state reachability answer.
///
/// `Unknown` is a valid, non-error result. Callers use it to fall back to the
/// always-correct slow path when the accelerator cannot prove `Yes` or `No`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reach {
    Yes,
    No,
    Unknown,
}

impl Reach {
    /// Returns `true` when the answer is definitive.
    #[inline]
    #[must_use]
    pub const fn is_definitive(self) -> bool {
        matches!(self, Self::Yes | Self::No)
    }

    /// Returns `true` when the accelerator proved reachability.
    #[inline]
    #[must_use]
    pub const fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }

    /// Returns `true` when the accelerator proved non-reachability.
    #[inline]
    #[must_use]
    pub const fn is_no(self) -> bool {
        matches!(self, Self::No)
    }

    /// Returns `true` when the caller should consult the slow path.
    #[inline]
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Minimal contract for a reachability accelerator.
///
/// Implementations may use live overlays, sealed segments, bridge sets, or any
/// other indexing strategy. The only requirement is that `Unknown` must be a
/// safe fallback, never a correctness failure.
pub trait Reachability {
    /// Event identifier type used by the accelerator.
    type Id: ?Sized;

    /// Returns whether `from` can reach `to`.
    ///
    /// The contract is intentionally asymmetric:
    /// - `Reach::Yes` and `Reach::No` are hard answers.
    /// - `Reach::Unknown` means "ask the slow path."
    fn reaches(&self, from: &Self::Id, to: &Self::Id) -> Reach;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;

    impl Reachability for Dummy {
        type Id = u32;

        fn reaches(&self, from: &Self::Id, to: &Self::Id) -> Reach {
            if from == to {
                Reach::Yes
            } else {
                Reach::Unknown
            }
        }
    }

    #[test]
    fn reach_helpers_reflect_the_variant() {
        assert!(Reach::Yes.is_definitive());
        assert!(Reach::No.is_definitive());
        assert!(!Reach::Unknown.is_definitive());
        assert!(Reach::Yes.is_yes());
        assert!(Reach::No.is_no());
        assert!(Reach::Unknown.is_unknown());
    }

    #[test]
    fn trait_contract_allows_unknown_fallback() {
        let accel = Dummy;
        assert_eq!(accel.reaches(&7, &7), Reach::Yes);
        assert_eq!(accel.reaches(&7, &8), Reach::Unknown);
    }
}
