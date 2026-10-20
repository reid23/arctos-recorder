//! Calibrating the rig from its previews: each lens from pictures of a
//! chessboard, and the cameras' orientations from features they both see.

pub mod alignment;
pub mod chessboard;
pub mod picture;
mod rotation;

/// One of the conditions for a calibration to be good enough.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub description: String,
    pub passed: bool,
}

impl Check {
    fn new(description: impl Into<String>, passed: bool) -> Check {
        Check {
            description: description.into(),
            passed,
        }
    }
}

/// Whether every check passed.
pub fn all_passed(checks: &[Check]) -> bool {
    checks.iter().all(|check| check.passed)
}
