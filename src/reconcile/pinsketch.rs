// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! `PinSketch` decoding over the MSC0501 GF(2^64) profile.

use alloc::{vec, vec::Vec};

use super::algebraic::gf64_mul;

type Polynomial = Vec<u64>;

pub(crate) fn decode(odd_syndromes: &[u64], max_elements: usize) -> Option<Vec<u64>> {
    let all = reconstruct_syndromes(odd_syndromes);
    let mut locator = berlekamp_massey(&all, max_elements)?;
    if locator.len() == 1 {
        return Some(Vec::new());
    }
    locator.reverse();
    let expected = locator.len().checked_sub(1)?;
    let mut roots = Vec::with_capacity(expected);
    find_roots(locator, &mut roots)?;
    debug_assert_eq!(roots.len(), expected);
    roots.sort_unstable();
    Some(roots)
}

fn reconstruct_syndromes(odd: &[u64]) -> Vec<u64> {
    let mut all = vec![0; odd.len().saturating_mul(2)];
    for (index, value) in odd.iter().copied().enumerate() {
        all[index.saturating_mul(2)] = value;
        all[index.saturating_mul(2).saturating_add(1)] = gf64_mul(all[index], all[index]);
    }
    all
}

fn berlekamp_massey(syndromes: &[u64], max_degree: usize) -> Option<Polynomial> {
    let mut current = vec![1];
    let mut previous = vec![1];
    let mut previous_discrepancy = 1;

    for (n, syndrome) in syndromes.iter().copied().enumerate() {
        let mut discrepancy = syndrome;
        for (i, coefficient) in current.iter().copied().enumerate().skip(1) {
            discrepancy ^= gf64_mul(syndromes[n.checked_sub(i)?], coefficient);
        }
        if discrepancy == 0 {
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
            current.resize(new_len, 0);
        }
        let scale = gf64_mul(discrepancy, gf64_inv(previous_discrepancy)?);
        for (i, coefficient) in previous.iter().copied().enumerate() {
            let target = i.checked_add(shift)?;
            current[target] ^= gf64_mul(scale, coefficient);
        }
        if swap {
            previous = old_current;
            previous_discrepancy = discrepancy;
        }
    }
    (current.last().copied().unwrap_or(0) != 0).then_some(current)
}

fn gf64_inv(value: u64) -> Option<u64> {
    if value == 0 {
        return None;
    }
    // a^(2^64-2), using a fixed square-and-multiply schedule.
    let mut result = 1;
    let exponent = u64::MAX - 1;
    for bit in (0..64).rev() {
        result = gf64_mul(result, result);
        if exponent & (1_u64 << bit) != 0 {
            result = gf64_mul(result, value);
        }
    }
    Some(result)
}

fn trim(poly: &mut Polynomial) {
    while poly.last() == Some(&0) {
        poly.pop();
    }
}

fn make_monic(poly: &mut Polynomial) -> Option<()> {
    let leading = *poly.last()?;
    if leading == 1 {
        return Some(());
    }
    let inverse = gf64_inv(leading)?;
    for coefficient in poly {
        *coefficient = gf64_mul(*coefficient, inverse);
    }
    Some(())
}

fn poly_mod(modulus: &[u64], value: &mut Polynomial) -> Option<()> {
    let modulus_degree = modulus.len().checked_sub(1)?;
    if modulus.last() != Some(&1) {
        return None;
    }
    while value.len() >= modulus.len() {
        let term = value.pop()?;
        if term != 0 {
            let offset = value.len().checked_sub(modulus_degree)?;
            for (index, coefficient) in modulus[..modulus_degree].iter().copied().enumerate() {
                value[offset.checked_add(index)?] ^= gf64_mul(term, coefficient);
            }
        }
    }
    trim(value);
    Some(())
}

fn poly_div(mut dividend: Polynomial, divisor: &[u64]) -> Option<Polynomial> {
    if divisor.last() != Some(&1) || dividend.len() < divisor.len() {
        return None;
    }
    let mut quotient = vec![0; dividend.len().checked_sub(divisor.len())?.checked_add(1)?];
    let divisor_degree = divisor.len().checked_sub(1)?;
    while dividend.len() >= divisor.len() {
        let term = dividend.pop()?;
        let position = dividend.len().checked_sub(divisor_degree)?;
        quotient[position] = term;
        if term != 0 {
            for (index, coefficient) in divisor[..divisor_degree].iter().copied().enumerate() {
                dividend[position.checked_add(index)?] ^= gf64_mul(term, coefficient);
            }
        }
    }
    trim(&mut quotient);
    Some(quotient)
}

fn poly_gcd(mut left: Polynomial, mut right: Polynomial) -> Option<Polynomial> {
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

fn poly_square(poly: &mut Polynomial) -> Option<()> {
    if poly.is_empty() {
        return Some(());
    }
    let new_len = poly.len().checked_mul(2)?.checked_sub(1)?;
    let old = core::mem::take(poly);
    poly.resize(new_len, 0);
    for (index, coefficient) in old.into_iter().enumerate() {
        poly[index.checked_mul(2)?] = gf64_mul(coefficient, coefficient);
    }
    Some(())
}

fn trace_mod(modulus: &[u64], parameter: u64) -> Option<Polynomial> {
    let mut trace = vec![0, parameter];
    for _ in 0..63 {
        poly_square(&mut trace)?;
        if trace.len() < 2 {
            trace.resize(2, 0);
        }
        trace[1] = parameter;
        poly_mod(modulus, &mut trace)?;
    }
    Some(trace)
}

fn solve_quadratic_form(target: u64) -> Option<u64> {
    // Solve the GF(2)-linear map z -> z^2 + z using a reduced 64x65 matrix.
    #[derive(Clone, Copy)]
    struct Row {
        coefficients: u64,
        rhs: bool,
    }

    let mut rows = [Row {
        coefficients: 0,
        rhs: false,
    }; 64];
    for column in 0..64 {
        let basis = 1_u64 << column;
        let image = gf64_mul(basis, basis) ^ basis;
        for (row, equation) in rows.iter_mut().enumerate() {
            if image & (1_u64 << row) != 0 {
                equation.coefficients |= 1_u64 << column;
            }
        }
    }
    for (row, equation) in rows.iter_mut().enumerate() {
        if target & (1_u64 << row) != 0 {
            equation.rhs = true;
        }
    }

    let mut rank = 0;
    for column in 0..64 {
        let pivot = (rank..64).find(|row| rows[*row].coefficients & (1_u64 << column) != 0);
        let Some(pivot) = pivot else { continue };
        rows.swap(rank, pivot);
        let pivot_row = rows[rank];
        for (row, equation) in rows.iter_mut().enumerate() {
            if row != rank && equation.coefficients & (1_u64 << column) != 0 {
                equation.coefficients ^= pivot_row.coefficients;
                equation.rhs ^= pivot_row.rhs;
            }
        }
        rank = rank.checked_add(1)?;
    }
    if rows.iter().any(|row| row.coefficients == 0 && row.rhs) {
        return None;
    }
    let mut solution = 0_u64;
    for row in rows.iter().take(rank) {
        debug_assert_ne!(row.coefficients, 0);
        let pivot = row.coefficients.trailing_zeros();
        if row.rhs {
            solution |= 1_u64 << pivot;
        }
    }
    (gf64_mul(solution, solution) ^ solution == target).then_some(solution)
}

fn find_roots(poly: Polynomial, roots: &mut Vec<u64>) -> Option<()> {
    let degree = poly.len().checked_sub(1)?;
    if degree == 0 {
        return Some(());
    }
    if degree == 1 {
        roots.push(poly[0]);
        return Some(());
    }
    if degree == 2 {
        let linear = poly[1];
        if linear == 0 {
            return None;
        }
        let inverse = gf64_inv(linear)?;
        let normalized = gf64_mul(poly[0], gf64_mul(inverse, inverse));
        let root = gf64_mul(solve_quadratic_form(normalized)?, linear);
        roots.push(root);
        roots.push(root ^ linear);
        return Some(());
    }

    let mut parameter = 1;
    for _ in 0..64 {
        let trace = trace_mod(&poly, parameter)?;
        let factor = poly_gcd(poly.clone(), trace)?;
        if factor.len() > 1 && factor.len() < poly.len() {
            let quotient = poly_div(poly, &factor)?;
            find_roots(factor, roots)?;
            find_roots(quotient, roots)?;
            return Some(());
        }
        parameter = gf64_mul(parameter, 2);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverses_roundtrip() {
        for value in [1, 2, 3, 0xdead_beef, u64::MAX] {
            assert_eq!(gf64_mul(value, gf64_inv(value).unwrap()), 1);
        }
    }

    #[test]
    fn decodes_small_sets() {
        for expected in [vec![1], vec![1, 2], vec![1, 2, 3], vec![1, 2, 3, 4]] {
            let mut odd = vec![0; expected.len()];
            for value in &expected {
                let squared = gf64_mul(*value, *value);
                let mut power = *value;
                for syndrome in &mut odd {
                    *syndrome ^= power;
                    power = gf64_mul(power, squared);
                }
            }
            assert_eq!(decode(&odd, expected.len()), Some(expected));
        }
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
        for size in 1..=24 {
            let mut expected = Vec::new();
            while expected.len() < size {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if state != 0 && !expected.contains(&state) {
                    expected.push(state);
                }
            }
            expected.sort_unstable();
            let mut odd = vec![0; size];
            for value in &expected {
                let squared = gf64_mul(*value, *value);
                let mut power = *value;
                for syndrome in &mut odd {
                    *syndrome ^= power;
                    power = gf64_mul(power, squared);
                }
            }
            assert_eq!(decode(&odd, size), Some(expected));
        }
    }

    #[test]
    fn empty_sketch_decodes_to_empty_set() {
        assert_eq!(decode(&[0; 4], 4), Some(Vec::new()));
    }

    #[test]
    fn polynomial_helpers_reject_invalid_inputs() {
        assert_eq!(gf64_inv(0), None);

        let mut value = vec![1, 2, 3];
        assert_eq!(poly_mod(&[1, 2], &mut value), None);
        assert_eq!(poly_div(vec![1], &[1, 1]), None);
        assert_eq!(poly_div(vec![1, 2], &[1, 2]), None);

        let mut empty = Vec::new();
        poly_square(&mut empty).unwrap();
        assert!(empty.is_empty());
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
        assert!(roots.is_empty());
        assert_eq!(find_roots(vec![1, 0, 1], &mut roots), None);
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
}
