//! Application-wide error type.

/// Top-level error enum used across rmsync modules.
#[derive(Debug)]
pub struct Error;

impl Error {
    /// Construct a placeholder error. Real variants land in later specs.
    pub fn new() -> Self {
        todo!()
    }
}
