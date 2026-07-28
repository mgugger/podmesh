//! Structured logging utilities for consistent log formatting across the codebase
//!
//! This module provides macros and utilities to eliminate duplicated logging patterns
//! and ensure consistent log formatting throughout the application.

/// Crypto-specific logging utilities
pub struct CryptoLogger;

impl CryptoLogger {
    /// Log encryption/decryption operations
    pub fn log_crypto_operation(operation: &str, success: bool, details: Option<&str>) {
        let status = if success { "successful" } else { "failed" };
        match details {
            Some(detail) => log::info!("crypto: {} {}: {}", operation, status, detail),
            None => log::info!("crypto: {} {}", operation, status),
        }
    }

    /// Log signature verification results
    pub fn log_signature_verification(valid: bool, peer: Option<&str>) {
        match (valid, peer) {
            (true, Some(peer_id)) => {
                log::debug!("signature verification successful for peer: {}", peer_id)
            }
            (true, None) => log::debug!("signature verification successful"),
            (false, Some(peer_id)) => {
                log::warn!("signature verification failed for peer: {}", peer_id)
            }
            (false, None) => log::warn!("signature verification failed"),
        }
    }
}
