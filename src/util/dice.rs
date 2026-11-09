//! Authoritative dice rolling.

use std::fmt;

use rand::Rng;
use thiserror::Error;

/// Valid polyhedral die sides.
const VALID_SIDES: &[u32] = &[4, 6, 8, 10, 12, 20, 100];

/// Error from invalid dice parameters.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DiceError {
    #[error("Invalid dice: d{0}. Use d4, d6, d8, d10, d12, d20, or d100")]
    InvalidSides(u32),
    #[error("Count must be between 1 and 10 (got {0})")]
    InvalidCount(u32),
}

/// Outcome of a dice roll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollResult {
    pub rolls: Vec<u32>,
    pub total: i32,
    pub sides: u32,
    pub count: u32,
    pub modifier: i32,
}

impl fmt::Display for RollResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}d{}", self.count, self.sides)?;
        if self.modifier > 0 {
            write!(f, "+{}", self.modifier)?;
        } else if self.modifier < 0 {
            write!(f, "{}", self.modifier)?;
        }
        write!(f, ": {}", self.total)
    }
}

/// D&D 5e ability modifier: `floor((score - 10) / 2)`.
///
/// Examples: 10→0, 12→+1, 8→-1, 7→-2.
pub fn ability_modifier(score: i32) -> i32 {
    // Widen before subtracting so i32::MIN/MAX scores cannot overflow.
    ((i64::from(score) - 10).div_euclid(2)) as i32
}

/// Roll `count` dice of `sides` faces, then add `modifier` to the sum.
///
/// - `sides` must be one of 4, 6, 8, 10, 12, 20, 100
/// - `count` must be in 1..=10
pub fn roll(sides: u32, count: u32, modifier: i32) -> Result<RollResult, DiceError> {
    if !VALID_SIDES.contains(&sides) {
        return Err(DiceError::InvalidSides(sides));
    }
    if !(1..=10).contains(&count) {
        return Err(DiceError::InvalidCount(count));
    }

    let mut rng = rand::rng();
    let rolls: Vec<u32> = (0..count).map(|_| rng.random_range(1..=sides)).collect();
    let sum: i32 = rolls.iter().map(|&r| r as i32).sum();
    let total = sum.saturating_add(modifier);

    Ok(RollResult {
        rolls,
        total,
        sides,
        count,
        modifier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn roll_rejects_invalid_sides() {
        assert_eq!(roll(7, 1, 0), Err(DiceError::InvalidSides(7)));
        assert_eq!(roll(3, 1, 0), Err(DiceError::InvalidSides(3)));
    }

    #[test]
    fn roll_rejects_invalid_count() {
        assert_eq!(roll(20, 0, 0), Err(DiceError::InvalidCount(0)));
        assert_eq!(roll(20, 11, 0), Err(DiceError::InvalidCount(11)));
    }

    #[test]
    fn roll_accepts_all_standard_sides() {
        for &sides in VALID_SIDES {
            let result = roll(sides, 1, 0).expect("valid");
            assert_eq!(result.sides, sides);
            assert_eq!(result.count, 1);
            assert_eq!(result.rolls.len(), 1);
            assert!((1..=sides).contains(&result.rolls[0]));
            assert_eq!(result.total, result.rolls[0] as i32);
        }
    }

    #[test]
    fn roll_applies_modifier() {
        let result = roll(20, 1, 5).unwrap();
        assert_eq!(result.total, result.rolls[0] as i32 + 5);
        assert_eq!(result.modifier, 5);

        let neg = roll(6, 2, -3).unwrap();
        let sum: i32 = neg.rolls.iter().map(|&r| r as i32).sum();
        assert_eq!(neg.total, sum - 3);
    }

    #[test]
    fn roll_count_bounds() {
        let one = roll(6, 1, 0).unwrap();
        assert_eq!(one.rolls.len(), 1);
        let ten = roll(6, 10, 0).unwrap();
        assert_eq!(ten.rolls.len(), 10);
    }

    #[test]
    fn ability_modifier_dnd5e() {
        assert_eq!(ability_modifier(10), 0);
        assert_eq!(ability_modifier(11), 0);
        assert_eq!(ability_modifier(12), 1);
        assert_eq!(ability_modifier(13), 1);
        assert_eq!(ability_modifier(14), 2);
        assert_eq!(ability_modifier(8), -1);
        assert_eq!(ability_modifier(9), -1);
        assert_eq!(ability_modifier(7), -2);
        assert_eq!(ability_modifier(1), -5);
        assert_eq!(ability_modifier(20), 5);
        assert_eq!(ability_modifier(0), -5);
    }

    #[test]
    fn ability_modifier_extremes_do_not_overflow() {
        assert_eq!(ability_modifier(i32::MAX), 1_073_741_818);
        assert_eq!(ability_modifier(i32::MIN), -1_073_741_829);
    }

    #[test]
    fn roll_saturates_huge_modifiers() {
        let high = roll(20, 10, i32::MAX).unwrap();
        assert_eq!(high.total, i32::MAX);
        let low = roll(20, 10, i32::MIN).unwrap();
        let sum: i32 = low.rolls.iter().map(|&r| r as i32).sum();
        assert_eq!(low.total, i32::MIN.saturating_add(sum));
    }

    #[test]
    fn display_formats_nicely() {
        let r = RollResult {
            rolls: vec![14],
            total: 19,
            sides: 20,
            count: 1,
            modifier: 5,
        };
        assert_eq!(format!("{r}"), "1d20+5: 19");

        let neg = RollResult {
            rolls: vec![3, 4],
            total: 5,
            sides: 6,
            count: 2,
            modifier: -2,
        };
        assert_eq!(format!("{neg}"), "2d6-2: 5");

        let plain = RollResult {
            rolls: vec![4],
            total: 4,
            sides: 6,
            count: 1,
            modifier: 0,
        };
        assert_eq!(format!("{plain}"), "1d6: 4");
    }
}
