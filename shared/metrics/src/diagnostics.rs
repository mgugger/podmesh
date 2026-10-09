#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

pub const DIAGNOSTIC_SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureCategory {
    InvalidCatalog,
    InvalidValue,
    NumericOverflow,
    Snapshot,
    Encoding,
    Listener,
}

impl FailureCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidCatalog => "invalid_catalog",
            Self::InvalidValue => "invalid_value",
            Self::NumericOverflow => "numeric_overflow",
            Self::Snapshot => "snapshot",
            Self::Encoding => "encoding",
            Self::Listener => "listener",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticAction {
    Silent,
    LogFirst {
        category: FailureCategory,
    },
    LogSummary {
        category: FailureCategory,
        suppressed_count: u64,
    },
}

#[derive(Clone, Debug, Default)]
pub struct DiagnosticSuppressor {
    first_logged: bool,
    suppressed_count: u64,
    next_summary: Option<Instant>,
}

impl DiagnosticSuppressor {
    pub fn observe(&mut self, category: FailureCategory, now: Instant) -> DiagnosticAction {
        if !self.first_logged {
            self.first_logged = true;
            self.next_summary = Some(now + DIAGNOSTIC_SUMMARY_INTERVAL);
            return DiagnosticAction::LogFirst { category };
        }

        self.suppressed_count = self.suppressed_count.saturating_add(1);
        if self.next_summary.is_some_and(|deadline| now >= deadline) {
            let suppressed_count = std::mem::take(&mut self.suppressed_count);
            self.next_summary = Some(now + DIAGNOSTIC_SUMMARY_INTERVAL);
            DiagnosticAction::LogSummary {
                category,
                suppressed_count,
            }
        } else {
            DiagnosticAction::Silent
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_failure_is_immediate_and_summaries_are_rate_limited() {
        let start = Instant::now();
        let mut suppressor = DiagnosticSuppressor::default();
        assert_eq!(
            suppressor.observe(FailureCategory::InvalidCatalog, start),
            DiagnosticAction::LogFirst {
                category: FailureCategory::InvalidCatalog
            }
        );
        assert_eq!(
            suppressor.observe(
                FailureCategory::InvalidValue,
                start + Duration::from_secs(59)
            ),
            DiagnosticAction::Silent
        );
        assert_eq!(
            suppressor.observe(
                FailureCategory::NumericOverflow,
                start + Duration::from_secs(60)
            ),
            DiagnosticAction::LogSummary {
                category: FailureCategory::NumericOverflow,
                suppressed_count: 2,
            }
        );
        assert_eq!(
            suppressor.observe(FailureCategory::Listener, start + Duration::from_secs(60)),
            DiagnosticAction::Silent
        );
    }
}
