use std::fmt;

/// Confidence in a belief, measured in basis points.
///
/// Integers, not floats, because a belief store is rebuilt by replaying
/// history: the same events must produce the same state on every machine and
/// every build, and floating point cannot promise that. One basis point is
/// 0.01%, so the range is 0 to 10,000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Confidence(u16);

/// Basis points in certainty.
pub const BASIS_POINTS: u16 = 10_000;

/// A confidence outside the representable range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfidenceError {
    basis_points: u16,
}

impl ConfidenceError {
    /// The rejected value, in basis points.
    pub const fn basis_points(self) -> u16 {
        self.basis_points
    }
}

impl fmt::Display for ConfidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} basis points is above certainty ({BASIS_POINTS})",
            self.basis_points
        )
    }
}

impl std::error::Error for ConfidenceError {}

impl Confidence {
    /// Held with no confidence at all.
    pub const NONE: Confidence = Confidence(0);

    /// Held with certainty.
    pub const CERTAIN: Confidence = Confidence(BASIS_POINTS);

    /// Builds a confidence from basis points, in `0..=10_000`.
    ///
    /// # Errors
    ///
    /// Fails above certainty, so an over-confident assertion cannot enter a
    /// belief through a bad decode or a unit mix-up.
    pub const fn from_basis_points(basis_points: u16) -> Result<Self, ConfidenceError> {
        if basis_points <= BASIS_POINTS {
            Ok(Confidence(basis_points))
        } else {
            Err(ConfidenceError { basis_points })
        }
    }

    /// Builds a confidence from a whole percentage, in `0..=100`.
    pub const fn from_percent(percent: u8) -> Result<Self, ConfidenceError> {
        if percent as u16 <= 100 {
            Ok(Confidence(percent as u16 * 100))
        } else {
            Err(ConfidenceError {
                basis_points: percent as u16 * 100,
            })
        }
    }

    /// The confidence in basis points.
    pub const fn basis_points(self) -> u16 {
        self.0
    }

    /// The more confident of the two.
    pub const fn max(self, other: Self) -> Self {
        if self.0 >= other.0 { self } else { other }
    }

    /// The less confident of the two.
    pub const fn min(self, other: Self) -> Self {
        if self.0 <= other.0 { self } else { other }
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Integer arithmetic: display must not be the one place a float sneaks
        // into a deterministic system.
        write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}
