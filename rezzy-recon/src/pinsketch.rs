// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! `PinSketch` decoding over the MSC4521 GF(2^64) profile.

use alloc::{vec, vec::Vec};

use crate::binary_field::BinaryField;

use super::algebraic::AlgebraicError;

type Polynomial<F> = Vec<F>;

const FIELD_BITS: usize = 64;
const MIXED_FACTOR_TRIALS: usize = 8;
const FACTOR_PARAMETER_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
/// Seed for the dense multiplier applied to mixed trial parameters when the
/// reduction polynomial is sparse (see `WorkModel::mixed_squarings`).
const DENSE_PARAMETER_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

const MAX_FACTOR_WORK_GF256: usize = 300_000_000;
/// Cost-model worst case for GF(2^256) recursion: the sum of
/// `single_call_work_ceiling(d)` for `d = 2..=256`. A chain of `(1, d-1)`
/// splits maximizes this model; the DP test below proves that for the model.
/// Constructing an input that reaches it is still open, and the 300M budget
/// is deliberately a practical policy ceiling rather than this bound.
#[cfg(test)]
const MAX_FACTOR_WORK_GF256_WORST_CASE: u64 = 5_148_287_480;
#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
const _: () = assert!((MAX_FACTOR_WORK_GF256 as u64) < MAX_FACTOR_WORK_GF256_WORST_CASE);
// KNOWN GAP, still open: this covers observed *balanced* recursive
// splitting, not a proven worst-case recursion bound.
//
// Measured directly via `find_roots_with_budget`'s consumption:
// - Typical degree-256 successful decodes (over both random and structured/
//   consecutive-value inputs) require ~9.0–9.3M work units.
// - A single node's full non-splitting 72-trial ladder at degree 256
//   (`single_call_work_ceiling(256)`) draws 10,092,544 units (~10.1M).
// Both remain within this 16.0M budget with measurable headroom.
//
// What remains an open theoretical gap is a pathological *chain* of nodes
// where each recursion step needs a full non-splitting ladder before splitting
// off a single root (e.g. a degenerate sequence of (1, d-1) splits).
// Evaluated using `single_call_work_ceiling(d)` summed over d = 2..=256,
// that hypothetical worst-case chain totals exactly 916,609,400 work units
// (~917M, ~57x this budget). Constructing an algebraic locator polynomial
// that forces this exact split sequence is an open problem; if encountered,
// `find_roots_with_budget` deducts cost with checked arithmetic and fails
// safe with `AlgebraicError::BudgetExhausted` (prompting client-side fallback).
// Thus, "degree 256 is decodable within MAX_FACTOR_WORK" applies to typical/
// practical inputs, while the absolute worst-case bound remains an open gap.
const MAX_FACTOR_WORK: usize = 16_000_000;

/// Work-cost model for one field width. The formulas are the existing
/// GF(2^64) model generalized from `FIELD_BITS` to `bits`; the GF(2^64)
/// instance reproduces the historical constants exactly.
#[derive(Clone, Copy)]
pub(crate) struct WorkModel {
    bits: usize,
    mixed_trials: usize,
    /// Squarings of `DENSE_PARAMETER_SEED` that build the dense constant each
    /// mixed trial parameter is multiplied by. Needed when the reduction polynomial is sparse: `Tr(a)` is zero
    /// for every low-degree `a`, so a 64-bit seed never splits small roots.
    mixed_squarings: usize,
    max_factor_work: usize,
}

impl WorkModel {
    pub(crate) const GF64: Self = Self {
        bits: FIELD_BITS,
        mixed_trials: MIXED_FACTOR_TRIALS,
        mixed_squarings: 0,
        max_factor_work: MAX_FACTOR_WORK,
    };

    /// GF(2^256) work ceiling. Degree-256 release measurements use
    /// 35,390,836 units for consecutive roots and 36,138,455 for
    /// pseudorandom roots. (Before the dense trial parameters below, the
    /// consecutive case took 232,591,912: the sparse reduction polynomial
    /// makes `Tr(a)` zero for every low-degree `a`, so small roots burned
    /// about 260 failed trials.) The ceiling leaves wide headroom over those
    /// samples but is not a worst-case factorization bound, and has not been
    /// re-derived; exhaustion fails safely.
    ///
    /// # Expected `no_std` Software Multiplication Cost
    /// On `no_std` platforms (or targets lacking hardware PCLMULQDQ / AVX-512),
    /// carry-less multiplication falls back to software bitwise algorithms
    /// (`clmul_raw_bitwise`), which is roughly 10–20× slower per operation.
    /// While hardware-accelerated decode reaches 300M units in ~3.6–7.5 CPU
    /// seconds, the same ceiling in `no_std` software can require several
    /// tens of seconds to minutes of CPU time before either completing or
    /// failing with `AlgebraicError::BudgetExhausted`. Integrations running in
    /// `no_std` or synchronous event loops must account for these long decode
    /// times or configure caller-side timeouts / fallback selection accordingly.
    pub(crate) const GF256: Self = Self {
        bits: 256,
        mixed_trials: MIXED_FACTOR_TRIALS,
        // A 64-bit seed reaches full width after three squarings (64 -> 512).
        mixed_squarings: 3,
        max_factor_work: MAX_FACTOR_WORK_GF256,
    };

    pub(crate) const fn max_factor_work(self) -> usize {
        self.max_factor_work
    }

    const fn factor_trials(self) -> Option<usize> {
        self.bits.checked_add(self.mixed_trials)
    }

    /// (Conservative: the table-based construction now does about half these
    /// multiplications, but work units are charged from this model so the
    /// ceilings and budgets stay unchanged.)
    ///
    /// One-time cost of `build_frobenius_basis` for a degree-`degree`
    /// polynomial: `BITS - 1` squaring+reduce rounds, each `O(degree^2)`.
    const fn frobenius_basis_cost(self, degree: usize) -> Option<usize> {
        let Some(squared) = degree.checked_mul(degree) else {
            return None;
        };
        let Some(rounds) = self.bits.checked_sub(1) else {
            return None;
        };
        squared.checked_mul(rounds)
    }

    /// Per-trial cost once the node's Frobenius basis is already built:
    /// `O(BITS * degree)` for the trace linear combination, plus
    /// `O(degree^2)` for `poly_gcd`.
    const fn factor_trial_cost_with_basis(self, degree: usize) -> Option<usize> {
        let Some(trace_cost) = self.bits.checked_mul(degree) else {
            return None;
        };
        let Some(gcd_cost) = degree.checked_mul(degree) else {
            return None;
        };
        trace_cost.checked_add(gcd_cost)
    }

    /// Cost of `poly_div` on a successful split: `O(degree^2)`.
    const fn split_cost(degree: usize) -> Option<usize> {
        degree.checked_mul(degree)
    }

    /// Upper bound on one `find_roots_with_budget` call for a locator of
    /// degree `degree`: the basis build plus a full trial ladder at that
    /// degree, plus one split.
    const fn single_call_work_ceiling(self, degree: usize) -> Option<usize> {
        let Some(basis) = self.frobenius_basis_cost(degree) else {
            return None;
        };
        let Some(per_trial) = self.factor_trial_cost_with_basis(degree) else {
            return None;
        };
        let Some(trials) = self.factor_trials() else {
            return None;
        };
        let Some(ladder) = per_trial.checked_mul(trials) else {
            return None;
        };
        let Some(split) = Self::split_cost(degree) else {
            return None;
        };
        let Some(subtotal) = basis.checked_add(ladder) else {
            return None;
        };
        subtotal.checked_add(split)
    }
}

pub(crate) fn decode(
    odd_syndromes: &[u64],
    max_elements: usize,
) -> Result<Vec<u64>, AlgebraicError> {
    let roots = decode_roots(prepare_locator(odd_syndromes, max_elements)?, find_roots)?;
    require_reencode(roots, odd_syndromes)
}

/// MSC4521 phase-1 re-encode check: re-encoding the recovered roots into a
/// temporary sketch MUST reproduce the residual syndromes ($O(k^2)$).
///
/// Done here, at the one place every caller decodes through, so the bucket,
/// `decode_elements` and strata-estimator paths all get it.
fn require_reencode<F: BinaryField>(
    roots: Vec<F>,
    odd_syndromes: &[F],
) -> Result<Vec<F>, AlgebraicError> {
    let mut check = vec![F::ZERO; odd_syndromes.len()];
    for &root in &roots {
        let squared = root.square();
        let mut odd_power = root;
        for coordinate in &mut check {
            *coordinate = coordinate.add(odd_power);
            odd_power = odd_power.mul(squared);
        }
    }
    (check == odd_syndromes)
        .then_some(roots)
        .ok_or(AlgebraicError::DecodeFailure)
}

/// Like [`decode`], but draws factoring work from `budget` and leaves the
/// unspent remainder in it, so a caller decoding multiple sketches can share
/// one budget across all of them instead of each call getting its own
/// implicit ceiling.
pub(crate) fn decode_with_budget(
    odd_syndromes: &[u64],
    max_elements: usize,
    budget: &mut usize,
) -> Result<Vec<u64>, AlgebraicError> {
    let roots = decode_roots(
        prepare_locator(odd_syndromes, max_elements)?,
        |locator, roots| find_roots_with_budget(locator, roots, budget),
    )?;
    require_reencode(roots, odd_syndromes)
}

/// Finds the roots of an already-prepared locator and finalizes them, or
/// returns the empty set when `prepared` is `None`.
fn decode_roots<F: BinaryField + Ord>(
    prepared: Option<(Polynomial<F>, usize)>,
    find: impl FnOnce(Polynomial<F>, &mut Vec<F>) -> Result<(), AlgebraicError>,
) -> Result<Vec<F>, AlgebraicError> {
    let Some((locator, expected)) = prepared else {
        return Ok(Vec::new());
    };
    let mut roots = Vec::with_capacity(expected);
    find(locator, &mut roots)?;
    finalize_roots(roots, expected)
}

/// Reconstructs syndromes, runs Berlekamp-Massey, and reverses the locator so
/// its roots can be found. Returns `None` when the residual decodes to the
/// empty set (`locator.len() == 1`), matching the early-return in [`decode`]
/// and [`decode_with_budget`].
fn prepare_locator<F: BinaryField>(
    odd_syndromes: &[F],
    max_elements: usize,
) -> Result<Option<(Polynomial<F>, usize)>, AlgebraicError> {
    let all = reconstruct_syndromes(odd_syndromes);
    let mut locator = berlekamp_massey(&all, max_elements).ok_or(AlgebraicError::DecodeFailure)?;
    if locator.len() == 1 {
        return Ok(None);
    }
    locator.reverse();
    let expected = locator
        .len()
        .checked_sub(1)
        .ok_or(AlgebraicError::DecodeFailure)?;
    Ok(Some((locator, expected)))
}

/// Rejects an incorrect root count or zero roots, then returns the roots sorted.
fn finalize_roots<F: BinaryField + Ord>(
    mut roots: Vec<F>,
    expected: usize,
) -> Result<Vec<F>, AlgebraicError> {
    if roots.len() != expected || roots.contains(&F::ZERO) {
        return Err(AlgebraicError::DecodeFailure);
    }
    roots.sort_unstable();
    Ok(roots)
}

/// Field-generic decode entry point used by the concrete wider field profile.
pub(crate) fn decode_with_model<F: BinaryField + Ord>(
    odd_syndromes: &[F],
    max_elements: usize,
    model: WorkModel,
    cached_solver: Option<&QuadraticSolver<F>>,
) -> Result<Vec<F>, AlgebraicError> {
    let mut work = model.max_factor_work();
    decode_with_model_budget(odd_syndromes, max_elements, model, cached_solver, &mut work)
}

/// Like [`decode_with_model`], but draws factoring work from `work` and leaves
/// the unspent remainder in it so a caller can share one budget across many
/// sketches.
pub(crate) fn decode_with_model_budget<F: BinaryField + Ord>(
    odd_syndromes: &[F],
    max_elements: usize,
    model: WorkModel,
    cached_solver: Option<&QuadraticSolver<F>>,
    work: &mut usize,
) -> Result<Vec<F>, AlgebraicError> {
    let roots = decode_roots(
        prepare_locator(odd_syndromes, max_elements)?,
        |locator, roots| find_roots_with_solver(locator, roots, work, model, cached_solver),
    )?;
    require_reencode(roots, odd_syndromes)
}

fn reconstruct_syndromes<F: BinaryField>(odd: &[F]) -> Vec<F> {
    let mut all = vec![F::ZERO; odd.len().saturating_mul(2)];
    for (index, value) in odd.iter().copied().enumerate() {
        all[index.saturating_mul(2)] = value;
        // Earlier iterations have already reconstructed the source at `index`.
        all[index.saturating_mul(2).saturating_add(1)] = all[index].mul(all[index]);
    }
    all
}

fn berlekamp_massey<F: BinaryField>(syndromes: &[F], max_degree: usize) -> Option<Vec<F>> {
    let mut current = vec![F::ONE];
    let mut previous = vec![F::ONE];
    let mut previous_discrepancy = F::ONE;

    for (n, syndrome) in syndromes.iter().copied().enumerate() {
        let mut discrepancy = syndrome;
        for (i, coefficient) in current.iter().copied().enumerate().skip(1) {
            let Some(syndrome_index) = n.checked_sub(i) else {
                continue;
            };
            discrepancy = discrepancy.add(syndromes[syndrome_index].mul(coefficient));
        }
        if discrepancy == F::ZERO {
            continue;
        }

        let current_degree = current.len().checked_sub(1)?;
        let previous_degree = previous.len().checked_sub(1)?;
        let shift = n
            .checked_add(1)?
            .checked_sub(current_degree)?
            .checked_sub(previous_degree)?;
        let swap = current_degree.checked_mul(2)? <= n;
        let old_current = current.clone();
        if swap {
            let new_len = previous.len().checked_add(shift)?;
            if new_len.checked_sub(1)? > max_degree {
                return None;
            }
            current.resize(new_len, F::ZERO);
        }
        let scale = discrepancy.mul(previous_discrepancy.inv()?);
        for (i, coefficient) in previous.iter().copied().enumerate() {
            let target = i.checked_add(shift)?;
            current[target] = current[target].add(scale.mul(coefficient));
        }
        if swap {
            previous = old_current;
            previous_discrepancy = discrepancy;
        }
    }
    (current.last().copied().unwrap_or(F::ZERO) != F::ZERO).then_some(current)
}

#[cfg(test)]
fn gf64_inv(value: u64) -> Option<u64> {
    BinaryField::inv(value)
}

fn trim<F: BinaryField>(poly: &mut Polynomial<F>) {
    while poly.last() == Some(&F::ZERO) {
        poly.pop();
    }
}

fn make_monic<F: BinaryField>(poly: &mut Polynomial<F>) -> Option<()> {
    let leading = *poly.last()?;
    if leading == F::ONE {
        return Some(());
    }
    let inverse = leading.inv()?;
    for coefficient in poly {
        *coefficient = (*coefficient).mul(inverse);
    }
    Some(())
}

fn poly_mod<F: BinaryField>(modulus: &[F], value: &mut Polynomial<F>) -> Option<()> {
    let modulus_degree = modulus.len().checked_sub(1)?;
    if modulus.last() != Some(&F::ONE) {
        return None;
    }
    while value.len() >= modulus.len() {
        let term = value.pop()?;
        if term != F::ZERO {
            let offset = value.len().checked_sub(modulus_degree)?;
            let target = &mut value[offset..offset.checked_add(modulus_degree)?];
            let source = &modulus[..modulus_degree];
            F::mac(term, source, target);
        }
    }
    trim(value);
    Some(())
}

fn poly_div<F: BinaryField>(mut dividend: Polynomial<F>, divisor: &[F]) -> Option<Polynomial<F>> {
    if divisor.last() != Some(&F::ONE) || dividend.len() < divisor.len() {
        return None;
    }
    let mut quotient = vec![F::ZERO; dividend.len().checked_sub(divisor.len())?.checked_add(1)?];
    let divisor_degree = divisor.len().checked_sub(1)?;
    while dividend.len() >= divisor.len() {
        let term = dividend.pop()?;
        let position = dividend.len().checked_sub(divisor_degree)?;
        quotient[position] = term;
        if term != F::ZERO {
            let target = &mut dividend[position..position.checked_add(divisor_degree)?];
            let source = &divisor[..divisor_degree];
            F::mac(term, source, target);
        }
    }
    trim(&mut quotient);
    Some(quotient)
}

#[cfg(test)]
fn poly_gcd<F: BinaryField>(
    mut left: Polynomial<F>,
    mut right: Polynomial<F>,
) -> Option<Polynomial<F>> {
    if left.len() < right.len() {
        core::mem::swap(&mut left, &mut right);
    }
    while !right.is_empty() {
        make_monic(&mut right)?;
        poly_mod(&right, &mut left)?;
        core::mem::swap(&mut left, &mut right);
    }
    make_monic(&mut left)?;
    Some(left)
}

/// Reusable buffers for the trial loop's gcd. Returns only the degree (as a
/// slice); the caller clones the factor only on the trial that splits.
struct GcdWorkspace<F> {
    left: Polynomial<F>,
    right: Polynomial<F>,
}

impl<F: BinaryField> GcdWorkspace<F> {
    const fn new() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    fn gcd(&mut self, dividend: &[F], divisor: &[F]) -> Option<&[F]> {
        self.left.clear();
        self.left.extend_from_slice(dividend);
        self.right.clear();
        self.right.extend_from_slice(divisor);
        if self.left.len() < self.right.len() {
            core::mem::swap(&mut self.left, &mut self.right);
        }
        while !self.right.is_empty() {
            make_monic(&mut self.right)?;
            poly_mod(&self.right, &mut self.left)?;
            core::mem::swap(&mut self.left, &mut self.right);
        }
        make_monic(&mut self.left)?;
        Some(&self.left)
    }
}

fn poly_square<F: BinaryField>(poly: &mut Polynomial<F>) -> Option<()> {
    if poly.is_empty() {
        return Some(());
    }
    let new_len = poly.len().checked_mul(2)?.checked_sub(1)?;
    let old = core::mem::take(poly);
    poly.resize(new_len, F::ZERO);
    for (target, coefficient) in (0..new_len).step_by(2).zip(old) {
        poly[target] = coefficient.square();
    }
    Some(())
}

/// Naive reference implementation of `Tr(parameter * X) mod modulus`,
/// re-deriving the full squaring recurrence from scratch for every
/// `parameter`. Cost is `O(TRACE_SQUARES * degree^2)` *per call*. Superseded
/// in the production path by `build_frobenius_basis` +
/// `trace_from_basis`, which amortize the squaring recurrence across every
/// trial for a given `modulus` instead of repeating it per trial (see that
/// pair's doc comments). Kept as a test-only cross-check that the
/// precomputed path computes the same trace.
#[cfg(test)]
fn trace_mod<F: BinaryField>(modulus: &[F], parameter: F) -> Option<Polynomial<F>> {
    let mut trace = vec![F::ZERO, parameter];
    for _ in 1..F::BITS {
        poly_square(&mut trace)?;
        if trace.len() < 2 {
            trace.resize(2, F::ZERO);
        }
        trace[1] = parameter;
        poly_mod(modulus, &mut trace)?;
    }
    Some(trace)
}

/// One-time-per-polynomial precomputation consumed by `trace_from_basis`:
/// the images `X^(2^0), X^(2^1), ..., X^(2^(BITS-1)) mod modulus`.
///
/// Squaring is GF(2)-linear in characteristic 2 (`(a+b)^2 = a^2+b^2`), so
/// `(parameter*X)^(2^i) = parameter^(2^i) * X^(2^i)` -- the trace
/// `sum_i (parameter*X)^(2^i) mod modulus` decomposes into
/// `sum_i parameter^(2^i) * B_i`, where `B_i = X^(2^i) mod modulus` does not
/// depend on `parameter` at all. Building this basis once per polynomial and
/// reusing it across every trial in `find_roots_with_budget`'s ladder turns
/// each trial's dominant cost from `O(TRACE_SQUARES * degree^2)` (the naive
/// `trace_mod`, repeating the squaring-and-reduce recurrence per trial) into
/// `O(FIELD_BITS * degree)` for the linear combination, leaving only the
/// unavoidable `O(degree^2)` GCD/division per trial. See
/// `frobenius_basis_cost` and `factor_trial_cost_with_basis`.
fn build_frobenius_basis<F: BinaryField>(modulus: &[F]) -> Option<Vec<Polynomial<F>>> {
    let degree = modulus.len().checked_sub(1)?;
    if modulus.last() != Some(&F::ONE) {
        return None;
    }
    if degree < 2 {
        return build_frobenius_basis_by_division(modulus);
    }
    let low = &modulus[..degree];

    // Squaring `s = sum s_j X^j` gives `sum s_j^2 X^(2j)`. Terms with
    // `2j < degree` need no reduction; only `X^(2j) mod f` for the upper half
    // `j >= degree/2` is needed, and that table is built once per node and
    // reused by every one of the `BITS - 1` squarings. Each squaring then costs
    // about `degree^2 / 2` multiplications instead of the `degree^2` of a
    // schoolbook division of the degree-`2*degree - 2` square.
    let first_high = degree.checked_add(1)? / 2;
    let mut power = low.to_vec(); // X^degree mod f (f is monic; char 2)
    if first_high.checked_mul(2)? > degree {
        mul_x_mod(&mut power, low)?;
    }
    let mut table = Vec::with_capacity(degree.checked_sub(first_high)?);
    for index in first_high..degree {
        table.push(power.clone());
        if index.checked_add(1)? < degree {
            mul_x_mod(&mut power, low)?;
            mul_x_mod(&mut power, low)?;
        }
    }

    let mut current = vec![F::ZERO; degree];
    *current.get_mut(1)? = F::ONE; // X (degree >= 2)
    let mut basis = Vec::with_capacity(F::BITS);
    basis.push(trimmed(&current));
    for _ in 1..F::BITS {
        let mut next = vec![F::ZERO; degree];
        for (index, coefficient) in current.iter().copied().enumerate() {
            if coefficient == F::ZERO {
                continue;
            }
            let square = coefficient.square();
            if index.checked_mul(2)? < degree {
                let slot = next.get_mut(index.checked_mul(2)?)?;
                *slot = slot.add(square);
            } else {
                F::mac(
                    square,
                    table.get(index.checked_sub(first_high)?)?,
                    &mut next,
                );
            }
        }
        basis.push(trimmed(&next));
        current = next;
    }
    Some(basis)
}

/// `value * X mod f` for a monic `f` whose coefficients below the leading
/// term are `low`; `value` has exactly `low.len()` coefficients.
fn mul_x_mod<F: BinaryField>(value: &mut [F], low: &[F]) -> Option<()> {
    let carry = *value.last()?;
    let len = value.len();
    value.copy_within(0..len.checked_sub(1)?, 1);
    *value.first_mut()? = F::ZERO;
    if carry != F::ZERO {
        F::mac(carry, low, value);
    }
    Some(())
}

fn trimmed<F: BinaryField>(value: &[F]) -> Polynomial<F> {
    let mut poly = value.to_vec();
    trim(&mut poly);
    poly
}

/// Schoolbook basis construction: repeated square-then-divide. Used for the
/// degenerate degrees the table method does not cover, and as the test
/// reference for it.
fn build_frobenius_basis_by_division<F: BinaryField>(modulus: &[F]) -> Option<Vec<Polynomial<F>>> {
    let mut basis = Vec::with_capacity(F::BITS);
    let mut current = vec![F::ZERO, F::ONE]; // X
    poly_mod(modulus, &mut current)?;
    basis.push(current.clone());
    for _ in 1..F::BITS {
        poly_square(&mut current)?;
        poly_mod(modulus, &mut current)?;
        basis.push(current.clone());
    }
    Some(basis)
}

/// Roots of a degree-2 locator `c + b X + X^2` (`b != 0`) via the linear
/// quadratic solver.
fn solve_quadratic_locator<F: BinaryField>(
    poly: &[F],
    solver: &QuadraticSolver<F>,
    roots: &mut Vec<F>,
) -> Result<(), AlgebraicError> {
    let linear = poly[1];
    if linear == F::ZERO {
        return Err(AlgebraicError::DecodeFailure);
    }
    let inverse = linear.inv().ok_or(AlgebraicError::DecodeFailure)?;
    let normalized = poly[0].mul(inverse.mul(inverse));
    let root = solver
        .solve(normalized)
        .ok_or(AlgebraicError::DecodeFailure)?
        .mul(linear);
    roots.push(root);
    roots.push(root.add(linear));
    Ok(())
}

/// Whether `modulus` (monic, degree >= 2) divides `X^(2^BITS) - X`, i.e. has
/// `degree` distinct roots in the field. `basis` is
/// `build_frobenius_basis(modulus)`, so its last element is `X^(2^(BITS-1))`;
/// one more squaring-and-reduce gives `X^(2^BITS) mod modulus`, which must be
/// `X`.
fn splits_completely<F: BinaryField>(basis: &[Polynomial<F>], modulus: &[F]) -> bool {
    let Some(last) = basis.last() else {
        return false;
    };
    let mut power = last.clone();
    if poly_square(&mut power).is_none() || poly_mod(modulus, &mut power).is_none() {
        return false;
    }
    power == [F::ZERO, F::ONE]
}

/// Computes `Tr(parameter * X) mod modulus` from a basis built by
/// `build_frobenius_basis` for the same `modulus`.
#[cfg(test)]
fn trace_from_basis<F: BinaryField>(basis: &[Polynomial<F>], parameter: F) -> Polynomial<F> {
    let mut trace = Vec::new();
    trace_from_basis_into(basis, parameter, &mut trace);
    trace
}

/// [`trace_from_basis`] into a reusable buffer.
fn trace_from_basis_into<F: BinaryField>(
    basis: &[Polynomial<F>],
    parameter: F,
    trace: &mut Polynomial<F>,
) {
    let width = basis.iter().map(Vec::len).max().unwrap_or(0);
    trace.clear();
    trace.resize(width, F::ZERO);
    let mut power = parameter;
    for basis_term in basis {
        if power != F::ZERO {
            for (coefficient, term) in trace.iter_mut().zip(basis_term.iter()) {
                *coefficient = coefficient.add(power.mul(*term));
            }
        }
        power = power.square();
    }
    trim(trace);
}

const fn next_factor_parameter(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^ (state << 17)
}

pub(crate) struct QuadraticSolver<F> {
    words: usize,
    /// Pivot column of each reduced row; rows `pivots.len()..BITS` reduce to
    /// zero coefficients, so their transformed rhs must be zero.
    pivots: Vec<usize>,
    /// Row operations recorded during elimination, `BITS` rows of `words`
    /// words each, so applying them to a target is `BITS * words` AND+parity.
    transforms: Vec<u64>,
    _field: core::marker::PhantomData<F>,
}

impl<F: BinaryField> QuadraticSolver<F> {
    pub(crate) fn new() -> Option<Self> {
        // Reduce the GF(2)-linear map z -> z^2 + z once. `transform` records
        // the row operations, so each later solve only applies the reduced
        // matrix to a target vector.
        #[derive(Clone)]
        struct WorkRow {
            coefficients: Vec<u64>,
            transform: Vec<u64>,
        }
        let bits = F::BITS;
        let words = bits.div_ceil(64);
        let mut rows = Vec::with_capacity(bits);
        for row in 0..bits {
            let mut transform = vec![0; words];
            transform[row / 64] |= 1_u64 << (row % 64);
            rows.push(WorkRow {
                coefficients: vec![0; words],
                transform,
            });
        }
        for column in 0..bits {
            let basis = F::bit(column)?;
            let image = basis.square().add(basis);
            for (row, equation) in rows.iter_mut().enumerate() {
                if image.test_bit(row) {
                    equation.coefficients[column / 64] |= 1_u64 << (column % 64);
                }
            }
        }
        let mut rank = 0;
        let mut pivots = Vec::with_capacity(bits);
        for column in 0..bits {
            let word = column / 64;
            let mask = 1_u64 << (column % 64);
            let Some(pivot) = (rank..bits).find(|&row| rows[row].coefficients[word] & mask != 0)
            else {
                continue;
            };
            rows.swap(rank, pivot);
            let pivot_row = rows[rank].clone();
            for (row, equation) in rows.iter_mut().enumerate() {
                if row != rank && equation.coefficients[word] & mask != 0 {
                    for (coefficient, pivot_coefficient) in equation
                        .coefficients
                        .iter_mut()
                        .zip(&pivot_row.coefficients)
                    {
                        *coefficient ^= *pivot_coefficient;
                    }
                    for (coefficient, pivot_coefficient) in
                        equation.transform.iter_mut().zip(&pivot_row.transform)
                    {
                        *coefficient ^= *pivot_coefficient;
                    }
                }
            }
            pivots.push(column);
            rank = rank.checked_add(1)?;
        }
        let mut transforms = Vec::with_capacity(bits.saturating_mul(words));
        for row in &rows {
            transforms.extend_from_slice(&row.transform);
        }
        Some(Self {
            words,
            pivots,
            transforms,
            _field: core::marker::PhantomData,
        })
    }

    fn solve(&self, target: F) -> Option<F> {
        let bytes = target.to_le_bytes();
        let bytes = bytes.as_ref();
        // Little-endian word `index` of the target, zero-padded past the end.
        let target_word = |index: usize| -> u64 {
            let mut word = [0_u8; 8];
            let start = index.saturating_mul(8);
            if let Some(chunk) = bytes.get(start..) {
                let len = chunk.len().min(8);
                word[..len].copy_from_slice(&chunk[..len]);
            }
            u64::from_le_bytes(word)
        };
        let mut solution = F::ZERO;
        for (row, transform) in self.transforms.chunks_exact(self.words).enumerate() {
            let parity = transform
                .iter()
                .enumerate()
                .fold(0_u32, |parity, (index, mask)| {
                    parity ^ ((mask & target_word(index)).count_ones() & 1)
                });
            if parity == 0 {
                continue;
            }
            // A set rhs bit on a pivot row selects that column; on a reduced
            // zero row it is an inconsistent equation.
            let column = *self.pivots.get(row)?;
            solution = solution.add(F::bit(column)?);
        }
        (solution.square().add(solution) == target).then_some(solution)
    }
}

#[cfg(test)]
fn solve_quadratic_form<F: BinaryField>(target: F) -> Option<F> {
    QuadraticSolver::<F>::new()?.solve(target)
}

/// The GF(2^64) solved form is field-wide, not call-specific. Cache it under
/// `std` so a single degree-2 leaf does not pay the build cost that only
/// amortizes across several leaves. The `no_std` build falls back to building
/// one solver per root-finding call.
#[cfg(feature = "std")]
fn cached_gf64_quadratic_solver() -> Option<&'static QuadraticSolver<u64>> {
    static SOLVER: std::sync::OnceLock<Option<QuadraticSolver<u64>>> = std::sync::OnceLock::new();
    SOLVER.get_or_init(QuadraticSolver::new).as_ref()
}

#[cfg(not(feature = "std"))]
fn cached_gf64_quadratic_solver() -> Option<&'static QuadraticSolver<u64>> {
    None
}

fn find_roots(poly: Polynomial<u64>, roots: &mut Vec<u64>) -> Result<(), AlgebraicError> {
    let mut work = WorkModel::GF64.max_factor_work();
    find_roots_with_budget(poly, roots, &mut work)
}

/// One-time cost of `build_frobenius_basis` for a degree-`degree`
/// polynomial: `FIELD_BITS - 1` squaring+reduce rounds, each `O(degree^2)`
/// -- the same per-round cost the naive `trace_mod` paid per trial, now
/// paid once per recursion node instead.
#[cfg(test)]
const fn frobenius_basis_cost(degree: usize) -> Option<usize> {
    WorkModel::GF64.frobenius_basis_cost(degree)
}

/// Per-trial cost once the node's Frobenius basis is already built:
/// `O(FIELD_BITS * degree)` for `trace_from_basis`'s linear combination,
/// plus `O(degree^2)` for `poly_gcd` (which runs a `poly_mod` internally).
/// This is charged on every trial, whether or not it finds a split --
/// `poly_div` is charged separately, only on the trial that actually
/// splits, via `split_cost`. (An earlier version folded `poly_div`'s cost
/// in here unconditionally on the theory that it kept this a safe upper
/// bound; it didn't just pad the bound, it overcharged every non-splitting
/// trial for work that never happened -- an average split takes ~3.7
/// trials, so that wasted roughly 2.7x this term's cost of budget per
/// node.)
#[cfg(test)]
const fn factor_trial_cost_with_basis(degree: usize) -> Option<usize> {
    WorkModel::GF64.factor_trial_cost_with_basis(degree)
}

/// Cost of `poly_div` on a successful split: `O(degree^2)`. Charged once,
/// only on the trial that actually splits -- see `factor_trial_cost_with_basis`.
#[cfg(test)]
const fn split_cost(degree: usize) -> Option<usize> {
    WorkModel::split_cost(degree)
}

/// Upper bound on the work one `find_roots_with_budget` call can need for a
/// locator of degree `degree`, covering the basis build plus a full
/// `FACTOR_TRIALS`-trial ladder at that degree alone.
///
/// This deliberately does **not** try to bound the cost of the recursive
/// splits below the root -- that recursion has no proven worst-case bound
/// (see `MAX_FACTOR_WORK`'s comment). It exists so a caller decoding many
/// sketches under one shared budget (`triage::decode_bucket_sketches`) can
/// clamp how much of that shared budget any single sketch's decode is
/// allowed to draw, so one pathological sketch can't starve the rest of a
/// batch by exhausting the shared pool before later, cheaper sketches are
/// even attempted. A sketch whose own recursion exceeds this ceiling will
/// simply hit `BudgetExhausted` for its own allotment -- which is the
/// intended outcome here, not a bug in this bound.
pub(crate) const fn single_call_work_ceiling(degree: usize) -> Option<usize> {
    WorkModel::GF64.single_call_work_ceiling(degree)
}

fn find_roots_with_budget(
    poly: Polynomial<u64>,
    roots: &mut Vec<u64>,
    work: &mut usize,
) -> Result<(), AlgebraicError> {
    find_roots_with_solver(
        poly,
        roots,
        work,
        WorkModel::GF64,
        cached_gf64_quadratic_solver(),
    )
}

/// Field-generic root finder. The field width comes from `model`; the
/// deterministic trial parameters are the historical u64 xorshift seeds for
/// the mixed prefix, then `F::bit` for the Frobenius rounds, so the GF(2^64)
/// sequence is unchanged.
#[cfg(test)]
fn find_roots_with_model<F: BinaryField>(
    poly: Polynomial<F>,
    roots: &mut Vec<F>,
    work: &mut usize,
    model: WorkModel,
) -> Result<(), AlgebraicError> {
    find_roots_with_solver(poly, roots, work, model, None)
}

fn find_roots_with_solver<F: BinaryField>(
    poly: Polynomial<F>,
    roots: &mut Vec<F>,
    work: &mut usize,
    model: WorkModel,
    cached_solver: Option<&QuadraticSolver<F>>,
) -> Result<(), AlgebraicError> {
    let mut pending = vec![poly];
    let mut is_root_locator = true;
    let mut workspace = GcdWorkspace::new();
    let mut trace = Vec::new();
    let mut owned_solver: Option<QuadraticSolver<F>> = None;
    while let Some(poly) = pending.pop() {
        let degree = poly
            .len()
            .checked_sub(1)
            .ok_or(AlgebraicError::DecodeFailure)?;
        if degree == 0 {
            continue;
        }
        if degree == 1 {
            roots.push(poly[0]);
            continue;
        }
        if degree == 2 {
            let solver = if let Some(solver) = cached_solver {
                solver
            } else {
                if owned_solver.is_none() {
                    owned_solver =
                        Some(QuadraticSolver::new().ok_or(AlgebraicError::DecodeFailure)?);
                }
                owned_solver.as_ref().ok_or(AlgebraicError::DecodeFailure)?
            };
            solve_quadratic_locator(&poly, solver, roots)?;
            continue;
        }

        let basis_cost = model
            .frobenius_basis_cost(degree)
            .ok_or(AlgebraicError::DecodeFailure)?;
        *work = work
            .checked_sub(basis_cost)
            .ok_or(AlgebraicError::BudgetExhausted)?;
        let basis = build_frobenius_basis(&poly).ok_or(AlgebraicError::DecodeFailure)?;
        if is_root_locator {
            // A locator that splits into distinct linear factors divides
            // `X^(2^BITS) - X`. Children of a splitting polynomial split too,
            // so this is checked once, on the original locator. Rejecting here
            // skips the whole futile trial ladder for over-capacity or
            // corrupt sketches, bounding their cost to one basis build.
            if !splits_completely(&basis, &poly) {
                return Err(AlgebraicError::DecodeFailure);
            }
            is_root_locator = false;
        }

        let trials = model.factor_trials().ok_or(AlgebraicError::DecodeFailure)?;
        let mut split = None;
        let mut mixed_parameter = FACTOR_PARAMETER_SEED;
        let dense_multiplier = if model.mixed_squarings == 0 {
            None
        } else {
            let mut dense = F::from_u64(DENSE_PARAMETER_SEED);
            for _ in 0..model.mixed_squarings {
                dense = dense.square();
            }
            Some(dense)
        };
        for trial in 0..trials {
            let parameter = if trial >= model.mixed_trials {
                let basis_bit = trial
                    .checked_sub(model.mixed_trials)
                    .ok_or(AlgebraicError::DecodeFailure)?;
                F::bit(basis_bit).ok_or(AlgebraicError::DecodeFailure)?
            } else {
                let parameter = F::from_u64(mixed_parameter);
                dense_multiplier.map_or(parameter, |dense| {
                    // Multiply by a fixed dense constant. Squaring alone is not
                    // enough: `Tr(a^8 * 1) = Tr(a)` is still zero, so roots
                    // that differ by 1 could never be split by a low-degree seed.
                    parameter.mul(dense)
                })
            };
            let cost = model
                .factor_trial_cost_with_basis(degree)
                .ok_or(AlgebraicError::DecodeFailure)?;
            *work = work
                .checked_sub(cost)
                .ok_or(AlgebraicError::BudgetExhausted)?;
            trace_from_basis_into(&basis, parameter, &mut trace);
            let factor = workspace
                .gcd(&poly, &trace)
                .ok_or(AlgebraicError::DecodeFailure)?;
            if factor.len() > 1 && factor.len() < poly.len() {
                let factor = factor.to_vec();
                // poly_div only runs on this, the one trial that actually
                // splits -- charged separately from the per-trial cost
                // above so trials that don't split aren't overcharged for
                // division work they never performed.
                let div_cost =
                    WorkModel::split_cost(degree).ok_or(AlgebraicError::DecodeFailure)?;
                *work = work
                    .checked_sub(div_cost)
                    .ok_or(AlgebraicError::BudgetExhausted)?;
                let quotient = poly_div(poly, &factor).ok_or(AlgebraicError::DecodeFailure)?;
                split = Some((factor, quotient));
                break;
            }
            if trial < model.mixed_trials {
                mixed_parameter = next_factor_parameter(mixed_parameter);
            }
        }
        let (factor, quotient) = split.ok_or(AlgebraicError::DecodeFailure)?;
        pending.push(quotient);
        pending.push(factor);
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::algebraic::gf64_mul;
    use crate::gf256::Gf256;
    use crate::gf8::Gf8;

    fn gf8_poly_from_roots(roots: &[Gf8]) -> Vec<Gf8> {
        let mut poly = vec![Gf8::ONE];
        for &root in roots {
            let len = poly.len().checked_add(1).expect("small test polynomial");
            let mut next = vec![Gf8::ZERO; len];
            for (index, coefficient) in poly.iter().enumerate() {
                let next_index = index.checked_add(1).expect("small test polynomial");
                next[index] = next[index].plus(coefficient.times(root));
                next[next_index] = next[next_index].plus(*coefficient);
            }
            poly = next;
        }
        poly
    }

    fn gf8_eval(poly: &[Gf8], point: Gf8) -> Gf8 {
        poly.iter().rev().fold(Gf8::ZERO, |acc, &coefficient| {
            acc.mul(point).add(coefficient)
        })
    }

    fn gf8_brute_force_roots(poly: &[Gf8]) -> Vec<Gf8> {
        (0..=u8::MAX)
            .map(Gf8::from_u8)
            .filter(|&point| gf8_eval(poly, point) == Gf8::ZERO)
            .collect()
    }

    fn gf8_work_model() -> WorkModel {
        WorkModel {
            bits: 8,
            mixed_trials: MIXED_FACTOR_TRIALS,
            mixed_squarings: 0,
            max_factor_work: 1_000_000,
        }
    }

    #[test]
    fn gf8_inverse_round_trips() {
        for value in 1..=u8::MAX {
            let value = Gf8::from_u8(value);
            let inverse = value.inv().expect("non-zero");
            assert_eq!(value * inverse, Gf8::ONE);
        }
    }

    #[test]
    fn gf8_quadratic_solver_matches_trace() {
        for target in 0..=u8::MAX {
            let target = Gf8::from_u8(target);
            let mut trace = Gf8::ZERO;
            let mut power = target;
            for _ in 0..8 {
                trace = trace.add(power);
                power = power.square();
            }
            match solve_quadratic_form(target) {
                Some(solution) => {
                    assert_eq!(trace, Gf8::ZERO, "solvable target {target:?} has trace 1");
                    assert_eq!(solution.square().add(solution), target);
                }
                None => assert_eq!(trace, Gf8::ONE, "unsolvable target {target:?} has trace 0"),
            }
        }
    }

    #[test]
    fn gf8_root_finding_matches_brute_force() {
        let cases: &[&[Gf8]] = &[
            &[],
            &[Gf8::from_u8(1)],
            &[Gf8::from_u8(3), Gf8::from_u8(5)],
            &[Gf8::from_u8(1), Gf8::from_u8(2), Gf8::from_u8(3)],
            &[
                Gf8::from_u8(1),
                Gf8::from_u8(0x53),
                Gf8::from_u8(0xa7),
                Gf8::from_u8(0xff),
            ],
        ];
        for roots in cases {
            let poly = gf8_poly_from_roots(roots);
            let mut expected = gf8_brute_force_roots(&poly);
            let mut work = gf8_work_model().max_factor_work();
            let mut found = Vec::new();
            find_roots_with_model(poly, &mut found, &mut work, gf8_work_model())
                .expect("GF(2^8) roots");
            expected.sort_unstable_by_key(|root| root.to_u8());
            found.sort_unstable_by_key(|root| root.to_u8());
            assert_eq!(found, expected);
        }
    }

    /// Field trace `Tr(x) = sum_{i=0}^{63} x^(2^i)` in GF(2^64). The sum is
    /// zero exactly when `z^2 + z = x` has a solution.
    fn gf64_trace(target: u64) -> u64 {
        let mut trace = 0_u64;
        let mut power = target;
        for _ in 0..u64::BITS {
            trace ^= power;
            power = gf64_mul(power, power);
        }
        trace
    }

    #[test]
    fn gf64_quadratic_solver_matches_trace() {
        let mut state = 0x1357_9bdf_2468_ace0_u64;
        let mut targets = vec![
            0_u64,
            1,
            2,
            u64::MAX,
            0x8000_0000_0000_0000,
            0xdead_beef_cafe_babe,
        ];
        for _ in 0..1_024 {
            state = next_factor_parameter(state);
            targets.push(state);
        }

        let solver = QuadraticSolver::<u64>::new().expect("GF(2^64) solver");
        let mut solvable = 0_usize;
        let mut unsolvable = 0_usize;
        for target in targets {
            let trace = gf64_trace(target);
            assert!(trace == 0 || trace == 1, "trace left GF(2): {target:#x}");
            let solved = solver.solve(target);
            assert_eq!(solved, solve_quadratic_form(target));
            if let Some(root) = solved {
                solvable = solvable.saturating_add(1);
                assert_eq!(trace, 0, "solvable target {target:#x} has trace 1");
                assert_eq!(gf64_mul(root, root) ^ root, target);
            } else {
                unsolvable = unsolvable.saturating_add(1);
                assert_eq!(trace, 1, "unsolvable target {target:#x} has trace 0");
            }
        }
        assert!(solvable > 0, "benchmark did not cover solvable targets");
        assert!(unsolvable > 0, "benchmark did not cover unsolvable targets");
    }

    /// Timing probe, not a pass/fail test. Run with
    /// `cargo test -p rezzy-recon --release --lib quadratic_solver_timing -- --ignored --nocapture`.
    #[cfg(feature = "std")]
    #[test]
    #[ignore = "timing probe"]
    fn quadratic_solver_timing() {
        use std::hint::black_box;
        use std::time::Instant;

        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let targets: Vec<u64> = (0..2_048)
            .map(|_| {
                state = next_factor_parameter(state);
                state
            })
            .collect();

        let build_start = Instant::now();
        let solver = QuadraticSolver::<u64>::new().expect("GF(2^64) solver");
        let build_time = build_start.elapsed();

        for &target in &targets {
            black_box(solver.solve(target));
        }
        let rounds: usize = 64;
        let start = Instant::now();
        for _ in 0..rounds {
            for &target in &targets {
                black_box(solver.solve(target));
            }
        }
        let reuse_time = start.elapsed();

        let cached = cached_gf64_quadratic_solver().expect("cached GF(2^64) solver");
        let start = Instant::now();
        for _ in 0..rounds {
            for &target in &targets {
                black_box(cached.solve(target));
            }
        }
        let cached_time = start.elapsed();

        let start = Instant::now();
        for _ in 0..rounds {
            for &target in &targets {
                black_box(solve_quadratic_form(target));
            }
        }
        let rebuild_time = start.elapsed();

        let calls = rounds.checked_mul(targets.len()).expect("small benchmark");
        let reuse_ns = reuse_time.as_nanos() / u128::try_from(calls).expect("non-zero");
        let cached_ns = cached_time.as_nanos() / u128::try_from(calls).expect("non-zero");
        let rebuild_ns = rebuild_time.as_nanos() / u128::try_from(calls).expect("non-zero");
        std::println!(
            "build {build_time:?}; per-call: local reuse {reuse_ns} ns, \
             cached {cached_ns} ns, rebuilt {rebuild_ns} ns over {calls} calls"
        );
    }

    /// Timing probe for whole root-finding calls: owned (built per call) vs
    /// cached quadratic solver, by locator degree. Run with
    /// `cargo test -p rezzy-recon --release --lib root_finding_solver_timing -- --ignored --nocapture`.
    #[cfg(feature = "std")]
    #[test]
    #[ignore = "timing probe"]
    fn root_finding_solver_timing() {
        use std::hint::black_box;
        use std::time::Instant;

        let cached = cached_gf64_quadratic_solver().expect("cached GF(2^64) solver");
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for degree in [2_usize, 3, 4, 8, 32, 64] {
            let roots = unique_random_elements(&mut state, degree);
            let mut locator = vec![1_u64];
            for &root in &roots {
                let mut next = vec![0_u64; locator.len() + 1];
                for (i, &c) in locator.iter().enumerate() {
                    next[i + 1] ^= c;
                    next[i] ^= gf64_mul(root, c);
                }
                locator = next;
            }
            let rounds = if degree >= 32 { 8 } else { 128 };
            let time = |solver: Option<&QuadraticSolver<u64>>| {
                let start = Instant::now();
                for _ in 0..rounds {
                    let mut found = Vec::new();
                    let mut work = usize::MAX;
                    find_roots_with_solver(
                        locator.clone(),
                        &mut found,
                        &mut work,
                        WorkModel::GF64,
                        solver,
                    )
                    .expect("roots");
                    black_box(found);
                }
                start.elapsed().as_micros() / rounds
            };
            let owned_us = time(None);
            let cached_us = time(Some(cached));
            std::println!("degree {degree}: owned {owned_us} us, cached {cached_us} us per call");
        }
    }

    /// Builds the odd `s1, s3, ...` syndromes for a set of field elements.
    fn odd_syndromes(elements: &[u64]) -> Vec<u64> {
        let mut odd = vec![0; elements.len()];
        for value in elements {
            let squared = gf64_mul(*value, *value);
            let mut power = *value;
            for syndrome in &mut odd {
                *syndrome ^= power;
                power = gf64_mul(power, squared);
            }
        }
        odd
    }

    fn odd_syndromes_field<F: BinaryField>(elements: &[F]) -> Vec<F> {
        let mut odd = vec![F::ZERO; elements.len()];
        for &value in elements {
            let squared = value.square();
            let mut power = value;
            for syndrome in &mut odd {
                *syndrome = syndrome.add(power);
                power = power.mul(squared);
            }
        }
        odd
    }

    /// Draws `size` distinct nonzero elements from the deterministic
    /// `next_factor_parameter` stream.
    fn unique_random_elements(state: &mut u64, size: usize) -> Vec<u64> {
        let mut expected = Vec::new();
        while expected.len() < size {
            let candidate = next_factor_parameter(*state);
            *state = candidate;
            if candidate != 0 && !expected.contains(&candidate) {
                expected.push(candidate);
            }
        }
        expected
    }

    #[test]
    fn inverses_roundtrip() {
        for value in [1, 2, 3, 0xdead_beef, u64::MAX] {
            assert_eq!(gf64_mul(value, gf64_inv(value).unwrap()), 1);
        }
    }

    #[test]
    fn decodes_small_sets() {
        for expected in [vec![1], vec![1, 2], vec![1, 2, 3], vec![1, 2, 3, 4]] {
            let odd = odd_syndromes(&expected);
            assert_eq!(decode(&odd, expected.len()), Ok(expected));
        }
    }

    #[test]
    fn gf256_field_decodes_a_small_set_end_to_end() {
        let expected = [1_u64, 2, 3].map(Gf256::from_u64);
        let odd = odd_syndromes_field(&expected);
        assert_eq!(
            Gf256::from_u64(2).mul(Gf256::from_u64(3)),
            Gf256::from_u64(6)
        );
        assert_eq!(
            require_reencode(expected.to_vec(), &odd),
            Ok(expected.to_vec())
        );
        assert_eq!(Gf256::decode(&odd, expected.len()), Ok(expected.to_vec()));
    }

    #[cfg(feature = "std")]
    #[test]
    #[ignore = "timing and work-model calibration probe"]
    fn gf256_root_finding_work_timing() {
        use std::hint::black_box;
        use std::time::Instant;

        let cached_solver = Some(crate::gf256::cached_quadratic_solver());
        for degree in [2_usize, 3, 4, 8, 16, 32, 64, 128, 256] {
            let expected: Vec<Gf256> = (1..=degree)
                .map(|value| Gf256::from_u64(u64::try_from(value).expect("small root index")))
                .collect();
            let odd = odd_syndromes_field(&expected);
            let rounds = if degree <= 4 { 3 } else { 1 };
            for (label, solver) in [("per-call", None), ("cached", cached_solver)] {
                let mut total_elapsed = core::time::Duration::ZERO;
                let mut observed_work = 0_usize;
                for _ in 0..rounds {
                    let (locator, _) = prepare_locator(&odd, degree)
                        .expect("locator preparation")
                        .expect("non-empty set has a locator");
                    let mut roots = Vec::new();
                    let mut remaining_work = 1_000_000_000_usize;
                    let started = Instant::now();
                    find_roots_with_solver(
                        locator,
                        &mut roots,
                        &mut remaining_work,
                        WorkModel::GF256,
                        solver,
                    )
                    .expect("roots within the one-billion-unit measurement allowance");
                    total_elapsed = total_elapsed.saturating_add(started.elapsed());
                    observed_work = 1_000_000_000_usize
                        .checked_sub(remaining_work)
                        .expect("root finding only consumes work");
                    assert_eq!(finalize_roots(roots, degree).unwrap(), expected);
                    black_box(observed_work);
                }
                std::println!(
                    "GF(2^256) root degree {degree} {label}: {:?}/call, {observed_work} work units",
                    total_elapsed / rounds
                );
            }
        }

        let mut state = 0x6a09_e667_f3bc_c909_u64;
        let mut random_roots = Vec::with_capacity(256);
        while random_roots.len() < 256 {
            let candidate = Gf256::from_limbs(core::array::from_fn(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            }));
            if candidate != Gf256::ZERO && !random_roots.contains(&candidate) {
                random_roots.push(candidate);
            }
        }
        random_roots.sort_unstable();
        let odd = odd_syndromes_field(&random_roots);
        let (locator, _) = prepare_locator(&odd, random_roots.len())
            .expect("random locator preparation")
            .expect("random non-empty set has a locator");
        let mut roots = Vec::new();
        let mut remaining_work = 1_000_000_000_usize;
        let started = Instant::now();
        find_roots_with_solver(
            locator,
            &mut roots,
            &mut remaining_work,
            WorkModel::GF256,
            cached_solver,
        )
        .expect("random roots within the one-billion-unit measurement allowance");
        let elapsed = started.elapsed();
        let observed_work = 1_000_000_000_usize
            .checked_sub(remaining_work)
            .expect("root finding only consumes work");
        assert_eq!(
            finalize_roots(roots, random_roots.len()).unwrap(),
            random_roots
        );
        std::println!(
            "GF(2^256) random root degree 256 cached: {elapsed:?}, {observed_work} work units"
        );
    }

    /// The table-based basis must equal the schoolbook square-then-divide
    /// basis for every degree parity, in every field.
    #[test]
    fn frobenius_basis_matches_division_reference() {
        fn random_monic<F: BinaryField>(
            degree: usize,
            state: &mut u64,
            make: impl Fn(u64) -> F,
        ) -> Vec<F> {
            let mut poly: Vec<F> = (0..degree)
                .map(|_| {
                    *state ^= *state << 13;
                    *state ^= *state >> 7;
                    *state ^= *state << 17;
                    make(*state)
                })
                .collect();
            poly.push(F::ONE);
            poly
        }
        let mut state = 0x1234_5678_9abc_def1_u64;
        for degree in 2..=9 {
            let modulus = random_monic(degree, &mut state, |v| {
                Gf8::from_u8(u8::try_from(v & 0xff).expect("masked to eight bits"))
            });
            assert_eq!(
                build_frobenius_basis(&modulus).unwrap(),
                build_frobenius_basis_by_division(&modulus).unwrap(),
                "gf8 degree {degree}"
            );
        }
        for degree in [2_usize, 3, 4, 7, 16, 17] {
            let modulus = random_monic(degree, &mut state, |v| v);
            assert_eq!(
                build_frobenius_basis(&modulus).unwrap(),
                build_frobenius_basis_by_division(&modulus).unwrap(),
                "gf64 degree {degree}"
            );
        }
        for degree in [2_usize, 3, 4, 5, 8, 15, 32] {
            let modulus = random_monic(degree, &mut state, Gf256::from_u64);
            assert_eq!(
                build_frobenius_basis(&modulus).unwrap(),
                build_frobenius_basis_by_division(&modulus).unwrap(),
                "gf256 degree {degree}"
            );
        }
        // Non-monic moduli are rejected, as `poly_mod` does.
        assert!(build_frobenius_basis(&[1_u64, 2, 3]).is_none());
    }

    /// A locator splits completely exactly when it divides `X^(2^BITS) - X`.
    #[test]
    fn splits_completely_accepts_products_of_roots_and_rejects_others() {
        let roots: Vec<Gf256> = [3_u64, 9, 27, 81, 243].map(Gf256::from_u64).to_vec();
        let mut product = vec![Gf256::ONE];
        for root in &roots {
            let mut next = vec![Gf256::ZERO; product.len() + 1];
            for (i, c) in product.iter().enumerate() {
                next[i + 1] = next[i + 1].add(*c);
                next[i] = next[i].add(c.mul(*root));
            }
            product = next;
        }
        let basis = build_frobenius_basis(&product).unwrap();
        assert!(splits_completely(&basis, &product));
        // Perturbing one coefficient almost surely breaks complete splitting.
        let mut broken = product;
        broken[1] = broken[1].add(Gf256::from_u64(5));
        let basis = build_frobenius_basis(&broken).unwrap();
        assert!(!splits_completely(&basis, &broken));
    }

    /// A non-splitting locator must be rejected after one basis build, not by
    /// burning the trial ladder.
    #[cfg(feature = "std")]
    #[test]
    fn non_splitting_locator_fails_without_exhausting_work() {
        // A non-splitting degree-16 locator constructed from arbitrary syndromes.
        let odd: Vec<Gf256> = (0..16).map(|i| Gf256::from_u64(31 + i)).collect();
        let (locator, _) = prepare_locator(&odd, 16)
            .expect("prepare_locator must succeed")
            .expect("fixture must produce a locator");
        let basis = build_frobenius_basis(&locator).expect("locator is monic");
        assert!(!splits_completely(&basis, &locator));
        let mut roots = Vec::new();
        let mut work = WorkModel::GF256.max_factor_work();
        let started = work;
        let outcome = find_roots_with_solver(
            locator,
            &mut roots,
            &mut work,
            WorkModel::GF256,
            Some(crate::gf256::cached_quadratic_solver()),
        );
        assert_eq!(outcome, Err(AlgebraicError::DecodeFailure));
        // Only the single basis build may be charged before rejection; any
        // trial-ladder work means the splitting guard did not short-circuit.
        assert_eq!(
            started - work,
            WorkModel::GF256
                .frobenius_basis_cost(16)
                .expect("degree-16 basis cost"),
            "spent {}",
            started - work
        );
    }

    #[test]
    fn factors_a_quadratic() {
        let mut roots = Vec::new();
        find_roots(vec![gf64_mul(1, 2), 1 ^ 2, 1], &mut roots).unwrap();
        roots.sort_unstable();
        assert_eq!(roots, [1, 2]);
    }

    #[test]
    fn decodes_deterministic_varied_sets() {
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        for size in (1..=24).chain([32, 64]) {
            let mut expected = unique_random_elements(&mut state, size);
            expected.sort_unstable();
            let odd = odd_syndromes(&expected);
            assert_eq!(decode(&odd, size), Ok(expected));
        }
    }

    /// `MAX_FACTOR_WORK`'s own comment documents an unproven gap: no proof
    /// bounds the recursive-splitting cost, only measurements against
    /// random and adversarial-consecutive-value degree-256 inputs, plus a
    /// from-first-principles computation of one specific pathological shape
    /// (a chain of (1, d-1) splits) that would exceed the budget ~57x over
    /// if it occurred. This test does not close that gap -- constructing an
    /// input that actually forces that exact recursive shape is a separate,
    /// nontrivial exercise in choosing a locator polynomial with a
    /// specific factorization structure over GF(2^64), not attempted here.
    ///
    /// What this *does* verify, on a real degree-256 decode: the
    /// documented "fails safe" claim. `decode_with_budget` with a budget
    /// far too small to complete any real decode must return
    /// `BudgetExhausted` cleanly -- not panic, not loop unboundedly, not
    /// return a wrong/partial root set -- for a large, real (not
    /// specially-crafted) input, not just the small inputs the other tests
    /// in this module use.
    #[test]
    fn budget_exhaustion_on_a_large_decode_fails_safe() {
        let mut state = 0x243f_6a88_85a3_08d3_u64;
        let size = 256;
        let expected = unique_random_elements(&mut state, size);
        let odd = odd_syndromes(&expected);

        // Nowhere near enough to complete a degree-256 decode (measured at
        // ~9-10M for a full decode/non-splitting ladder per this file's own
        // comment) -- budget exhaustion must trigger, and trigger cleanly.
        let mut budget = 1_000;
        assert_eq!(
            decode_with_budget(&odd, size, &mut budget),
            Err(AlgebraicError::BudgetExhausted)
        );
    }

    #[test]
    fn decodes_a_triple_unsplit_by_the_mixed_parameter_prefix() {
        let expected = vec![1, 0xcd2, 0x1_d71a];
        let odd = odd_syndromes(&expected);
        assert_eq!(decode(&odd, expected.len()), Ok(expected));
    }

    #[test]
    fn empty_sketch_decodes_to_empty_set() {
        assert_eq!(decode(&[0; 4], 4), Ok(Vec::new()));
    }

    #[test]
    fn polynomial_helpers_reject_invalid_inputs() {
        assert_eq!(gf64_inv(0), None);

        let mut value = vec![1, 2, 3];
        assert_eq!(poly_mod(&[1, 2], &mut value), None);
        assert_eq!(poly_div(vec![1], &[1, 1]), None);
        assert_eq!(poly_div(vec![1, 2], &[1, 2]), None);
        assert_eq!(poly_div(vec![2, 3, 1], &[1, 1]), Some(vec![2, 1]));
        assert_eq!(poly_div(vec![0, 1, 1], &[1, 1]), Some(vec![0, 1]));

        let mut empty: Vec<u64> = Vec::new();
        poly_square(&mut empty).unwrap();
        assert_eq!(empty, [] as [u64; 0]);
    }

    #[test]
    fn gcd_swaps_lower_degree_left_operand() {
        assert_eq!(poly_gcd(vec![1, 1], vec![1, 0, 1]), Some(vec![1, 1]));
    }

    #[test]
    fn trace_handles_constant_modulus() {
        assert_eq!(trace_mod(&[1], 1), Some(Vec::new()));
    }

    #[test]
    fn root_finding_handles_constant_and_inseparable_polynomials() {
        let mut roots = Vec::new();
        find_roots(vec![1], &mut roots).unwrap();
        assert_eq!(roots, [] as [u64; 0]);
        assert_eq!(
            find_roots(vec![1, 0, 1], &mut roots),
            Err(AlgebraicError::DecodeFailure)
        );
        assert_eq!(
            find_roots(vec![1, 1, 0, 1], &mut roots),
            Err(AlgebraicError::DecodeFailure)
        );
    }

    #[test]
    fn root_finding_stops_when_its_work_budget_is_exhausted() {
        let pair_products = gf64_mul(1, 2) ^ gf64_mul(1, 3) ^ gf64_mul(2, 3);
        let polynomial = vec![gf64_mul(gf64_mul(1, 2), 3), pair_products, 0, 1];
        let mut roots = Vec::new();
        assert_eq!(
            find_roots_with_budget(polynomial, &mut roots, &mut 0),
            Err(AlgebraicError::BudgetExhausted)
        );
        assert_eq!(roots, [] as [u64; 0]);
    }

    #[test]
    fn maximum_degree_trace_exceeds_the_absolute_work_budget() {
        // The one-time basis build alone already exceeds the budget at
        // this degree (per-trial cost is deliberately small -- that's the
        // whole point of the precompute -- so it wouldn't on its own).
        assert!(frobenius_basis_cost(1_000).unwrap() > MAX_FACTOR_WORK);
        // The combined single-call ceiling (basis + full trial ladder +
        // one split) pins both halves of the cost model together.
        assert!(single_call_work_ceiling(1_000).unwrap() > MAX_FACTOR_WORK);
    }

    #[test]
    fn factor_trial_and_split_costs_are_pinned_independently() {
        // The combined ceiling above can pass even if one of its two
        // components silently drifts, as long as the other compensates.
        // Pin each component's exact value at a fixed degree so a
        // regression in either (e.g. factor_trial_cost_with_basis
        // accidentally including a term it shouldn't, or split_cost
        // losing one) shows up here even if single_call_work_ceiling's
        // sum still happens to clear MAX_FACTOR_WORK. Values are well
        // under MAX_FACTOR_WORK individually and are not meant to be --
        // see the doc comment on factor_trial_cost_with_basis for why
        // the per-trial cost is deliberately small.
        assert_eq!(factor_trial_cost_with_basis(1_000), Some(1_064_000));
        assert_eq!(split_cost(1_000), Some(1_000_000));
    }

    #[test]
    fn frobenius_basis_matches_naive_trace_for_every_parameter() {
        // A degree-4 modulus (x^4 + x + 1, i.e. coefficients low-to-high
        // [1, 1, 0, 0, 1]) is enough to exercise every basis position for a
        // handful of parameters, checked against the O(d^2)-per-call
        // reference implementation.
        let modulus = vec![1, 1, 0, 0, 1];
        let basis = build_frobenius_basis(&modulus).unwrap();
        for parameter in [0, 1, 2, 3, 0xdead_beef, u64::MAX] {
            assert_eq!(
                trace_from_basis(&basis, parameter),
                trace_mod(&modulus, parameter).unwrap(),
                "mismatch at parameter={parameter}"
            );
        }
    }

    #[test]
    fn quadratic_solver_rejects_an_inconsistent_target() {
        let target = (0..64)
            .map(|bit| 1_u64 << bit)
            .find(|target| {
                let mut trace = *target;
                let mut power = *target;
                for _ in 1..64 {
                    power = gf64_mul(power, power);
                    trace ^= power;
                }
                trace == 1
            })
            .expect("the absolute trace is a nonzero linear map");
        assert_eq!(solve_quadratic_form(target), None);
    }

    #[test]
    fn single_call_work_ceiling_matches_derivation() {
        // Degree 256 single-node ceiling:
        // frobenius_basis_cost(256) = 256^2 * 63 = 4,128,768
        // factor_trial_cost_with_basis(256) = 64 * 256 + 256^2 = 81,920
        // ladder (72 trials) = 72 * 81,920 = 5,898,240
        // split_cost(256) = 256^2 = 65,536
        // Total = 4,128,768 + 5,898,240 + 65,536 = 10,092,544 (~10.09M)
        assert_eq!(single_call_work_ceiling(256), Some(10_092_544));
    }

    #[test]
    fn theoretical_degenerate_chain_cost_evaluation() {
        // Sum of single_call_work_ceiling(d) for d in 2..=256:
        // sum_{d=2}^{256} [63 d^2 + 72 (64 d + d^2) + d^2] = 916,609,400 (~917M)
        let total_degenerate_work: u64 = (2..=256)
            .map(|d| single_call_work_ceiling(d).expect("does not overflow") as u64)
            .sum();
        assert_eq!(total_degenerate_work, 916_609_400);
    }

    #[test]
    fn gf256_work_model_worst_case_matches_degenerate_chain() {
        let mut worst = vec![0_u64; 257];
        for degree in 2_usize..=256 {
            let split = (1..degree)
                .map(|left| {
                    let right = degree.checked_sub(left).expect("left is below degree");
                    worst[left]
                        .checked_add(worst[right])
                        .expect("split subtree cost fits u64")
                })
                .max()
                .expect("degree two has one split");
            worst[degree] = WorkModel::GF256
                .single_call_work_ceiling(degree)
                .map(|cost| cost as u64)
                .and_then(|cost| cost.checked_add(split))
                .expect("worst-case cost fits u64");
        }
        let chain: u64 = (2..=256)
            .map(|degree| {
                WorkModel::GF256
                    .single_call_work_ceiling(degree)
                    .expect("does not overflow") as u64
            })
            .sum();
        assert_eq!(worst[256], chain);
        assert_eq!(worst[256], MAX_FACTOR_WORK_GF256_WORST_CASE);
    }

    #[test]
    fn consecutive_elements_decode_within_budget() {
        // Structured consecutive values (1..=N) produce deterministic syndromic
        // sketches that decode within the normal work budget.
        for count in [4, 8, 16, 32] {
            let mut odd_syndromes = vec![0_u64; count];
            for i in 1..=count {
                let elem = i as u64;
                let mut power = elem;
                for syn in &mut odd_syndromes {
                    *syn ^= power;
                    power = gf64_mul(power, gf64_mul(elem, elem)); // elem^(2k+1)
                }
            }
            let recovered = decode(&odd_syndromes, count).expect("decodes successfully");
            let mut expected: Vec<u64> = (1..=count as u64).collect();
            let mut sorted_recovered = recovered;
            expected.sort_unstable();
            sorted_recovered.sort_unstable();
            assert_eq!(sorted_recovered, expected);
        }
    }

    #[test]
    fn reencode_accepts_the_true_set_and_rejects_anything_else() {
        let mut odd = vec![0_u64; 4];
        for root in [3_u64, 9] {
            let squared = gf64_mul(root, root);
            let mut power = root;
            for coordinate in &mut odd {
                *coordinate ^= power;
                power = gf64_mul(power, squared);
            }
        }
        assert_eq!(require_reencode(vec![3, 9], &odd), Ok(vec![3, 9]));
        assert_eq!(
            require_reencode(vec![3, 10], &odd),
            Err(AlgebraicError::DecodeFailure)
        );
        // Unaccounted syndromes beyond the decoded roots are caught too.
        assert_eq!(
            require_reencode(vec![3], &odd),
            Err(AlgebraicError::DecodeFailure)
        );
        assert_eq!(require_reencode(Vec::new(), &[0, 0]), Ok(Vec::new()));
    }
}
