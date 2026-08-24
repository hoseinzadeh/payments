//! Penny-exact distribution of an amount across several recipients.
//!
//! Splitting money is the single most common source of off-by-one-cent bugs in
//! marketplace systems. Every split in this crate goes through [`allocate`],
//! which uses the *largest remainder method*: each part gets the floor of its
//! proportional share and the leftover minor units are handed out one at a time
//! to the parts with the biggest fractional remainder (ties broken by index, so
//! the result is deterministic). The parts are guaranteed to sum **exactly**
//! back to the input.

use crate::error::{Error, Result};
use crate::money::Money;

/// Split `total` proportionally to `weights`.
///
/// * Returns one [`Money`] per weight, in the same order.
/// * `sum(result) == total` always holds.
/// * Negative weights are rejected; a zero weight receives zero unless *all*
///   weights are zero, in which case the total is split as evenly as possible.
///
/// ```
/// use payments::money::{allocate, Currency, Money};
///
/// let total = Money::from_minor(100, Currency::USD);
/// let parts = allocate(total, &[1, 1, 1]).unwrap();
/// assert_eq!(parts.iter().map(|m| m.minor()).collect::<Vec<_>>(), vec![34, 33, 33]);
/// ```
pub fn allocate(total: Money, weights: &[i64]) -> Result<Vec<Money>> {
    if weights.is_empty() {
        return if total.is_zero() {
            Ok(Vec::new())
        } else {
            Err(Error::allocation("cannot allocate a non-zero amount to zero recipients"))
        };
    }
    if weights.iter().any(|w| *w < 0) {
        return Err(Error::allocation("allocation weights must be non-negative"));
    }

    let sum: i128 = weights.iter().map(|w| *w as i128).sum();
    if sum == 0 {
        return allocate_evenly(total, weights.len());
    }

    let currency = total.currency();
    let amount = total.minor() as i128;
    // Work on the magnitude so flooring behaves symmetrically for refunds
    // (negative amounts) and charges (positive amounts).
    let sign: i128 = if amount < 0 { -1 } else { 1 };
    let magnitude = amount * sign;

    let mut parts = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    let mut distributed: i128 = 0;

    for (index, weight) in weights.iter().enumerate() {
        let numerator = magnitude * (*weight as i128);
        let share = numerator.div_euclid(sum);
        let remainder = numerator.rem_euclid(sum);
        distributed += share;
        parts.push(share);
        remainders.push((remainder, index));
    }

    // Hand the leftover units to the largest remainders first.
    let mut leftover = magnitude - distributed;
    debug_assert!(leftover >= 0 && leftover < weights.len() as i128 + 1);
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut cursor = 0usize;
    while leftover > 0 && !remainders.is_empty() {
        let (_, index) = remainders[cursor % remainders.len()];
        parts[index] += 1;
        leftover -= 1;
        cursor += 1;
    }

    parts
        .into_iter()
        .map(|p| {
            i64::try_from(p * sign)
                .map(|minor| Money::from_minor(minor, currency))
                .map_err(|_| Error::allocation("allocated share out of range"))
        })
        .collect()
}

/// Split `total` into `parts` shares that differ by at most one minor unit.
pub fn allocate_evenly(total: Money, parts: usize) -> Result<Vec<Money>> {
    if parts == 0 {
        return if total.is_zero() {
            Ok(Vec::new())
        } else {
            Err(Error::allocation("cannot allocate a non-zero amount to zero recipients"))
        };
    }
    allocate(total, &vec![1i64; parts])
}

/// Split `total` proportionally to a set of [`Money`] weights (e.g. line subtotals).
///
/// All weights must share `total`'s currency.
pub fn allocate_by_weights(total: Money, weights: &[Money]) -> Result<Vec<Money>> {
    let mut numeric = Vec::with_capacity(weights.len());
    for weight in weights {
        total.assert_same_currency(*weight)?;
        if weight.is_negative() {
            return Err(Error::allocation("allocation weights must be non-negative"));
        }
        numeric.push(weight.minor());
    }
    allocate(total, &numeric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;

    fn minors(parts: &[Money]) -> Vec<i64> {
        parts.iter().map(|m| m.minor()).collect()
    }

    #[test]
    fn largest_remainder_is_exact() {
        let total = Money::from_minor(1_000, Currency::USD);
        let parts = allocate(total, &[333, 333, 334]).unwrap();
        assert_eq!(minors(&parts).iter().sum::<i64>(), 1_000);
    }

    #[test]
    fn indivisible_amounts_are_deterministic() {
        let total = Money::from_minor(10, Currency::USD);
        assert_eq!(minors(&allocate(total, &[1, 1, 1]).unwrap()), vec![4, 3, 3]);
        assert_eq!(minors(&allocate(total, &[1, 1, 1]).unwrap()), vec![4, 3, 3]);
    }

    #[test]
    fn negative_totals_split_symmetrically() {
        let refund = Money::from_minor(-10, Currency::USD);
        let parts = allocate(refund, &[1, 1, 1]).unwrap();
        assert_eq!(minors(&parts), vec![-4, -3, -3]);
        assert_eq!(minors(&parts).iter().sum::<i64>(), -10);
    }

    #[test]
    fn zero_weights_receive_nothing() {
        let total = Money::from_minor(500, Currency::USD);
        let parts = allocate(total, &[0, 5, 5]).unwrap();
        assert_eq!(minors(&parts), vec![0, 250, 250]);
    }

    #[test]
    fn all_zero_weights_split_evenly() {
        let total = Money::from_minor(7, Currency::USD);
        let parts = allocate(total, &[0, 0, 0]).unwrap();
        assert_eq!(minors(&parts).iter().sum::<i64>(), 7);
    }

    #[test]
    fn rejects_negative_weights() {
        let total = Money::from_minor(100, Currency::USD);
        assert!(allocate(total, &[-1, 2]).is_err());
    }

    #[test]
    fn exhaustive_sum_invariant() {
        for total in 0..=200i64 {
            for weights in [
                vec![1, 2, 3],
                vec![7, 11, 13, 17],
                vec![1, 0, 0, 1],
                vec![999, 1],
                vec![5],
            ] {
                let money = Money::from_minor(total, Currency::USD);
                let parts = allocate(money, &weights).unwrap();
                assert_eq!(
                    parts.iter().map(|m| m.minor()).sum::<i64>(),
                    total,
                    "weights {weights:?} total {total}"
                );
            }
        }
    }
}
