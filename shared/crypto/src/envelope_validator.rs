//! Centralized envelope validation utilities
//!
//! This module provides envelope validation types and configuration.
//! The actual validation logic is implemented in the p2p crate's envelope module
//! to avoid circular dependencies with the protocol crate.

use log::warn;

/// Envelope validation error types
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("Envelope verification failed: {0}")]
    VerificationFailed(String),
    #[error("Unsigned envelope rejected: {0}")]
    UnsignedRejected(String),
    #[error("Invalid envelope format: {0}")]
    InvalidFormat(String),
}

/// Direction of envelope validation (for logging)
#[derive(Debug, Clone, Copy)]
pub enum ValidationDirection {
    Inbound,
    Outbound,
}

impl std::fmt::Display for ValidationDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationDirection::Inbound => write!(f, "inbound"),
            ValidationDirection::Outbound => write!(f, "outbound"),
        }
    }
}

/// Centralized envelope validator configuration
pub struct EnvelopeValidator;

impl EnvelopeValidator {
    /// Check if signed messages are required.
    /// In production, this always returns true for security.
    pub fn require_signed_messages() -> bool {
        true
    }

    /// Log a rejected envelope for security auditing
    pub fn log_rejection(protocol: &str, direction: ValidationDirection, reason: &str) {
        warn!(
            "Envelope rejected: {} {} request - {}",
            direction, protocol, reason
        );
    }
}

/// Convenient type alias for envelope validation results
pub type ValidationResult<T> = Result<T, EnvelopeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_require_signed_messages() {
        assert!(EnvelopeValidator::require_signed_messages());
    }

    #[test]
    fn test_validation_direction_display() {
        assert_eq!(format!("{}", ValidationDirection::Inbound), "inbound");
        assert_eq!(format!("{}", ValidationDirection::Outbound), "outbound");
    }

    #[test]
    fn test_envelope_error_display() {
        let error = EnvelopeError::VerificationFailed("test error".to_string());
        assert!(error.to_string().contains("test error"));

        let error = EnvelopeError::UnsignedRejected("unsigned".to_string());
        assert!(error.to_string().contains("unsigned"));

        let error = EnvelopeError::InvalidFormat("format".to_string());
        assert!(error.to_string().contains("format"));
    }
}
