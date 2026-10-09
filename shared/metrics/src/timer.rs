#![forbid(unsafe_code)]

use std::time::Instant;

use crate::{
    catalog::{OperationName, Outcome, Reason},
    recorder::Metrics,
};

#[derive(Debug)]
pub struct OperationTimer {
    metrics: Option<Metrics>,
    operation: Option<OperationName>,
    started: Option<Instant>,
}

impl OperationTimer {
    pub(crate) fn noop() -> Self {
        Self {
            metrics: None,
            operation: None,
            started: None,
        }
    }

    pub(crate) fn registered(metrics: Metrics, operation: OperationName) -> Self {
        Self {
            metrics: Some(metrics),
            operation: Some(operation),
            started: Some(Instant::now()),
        }
    }

    pub fn finish(mut self, outcome: Outcome, reason: Reason) {
        self.complete(outcome, reason);
    }

    fn complete(&mut self, outcome: Outcome, reason: Reason) {
        let (Some(metrics), Some(operation), Some(started)) = (
            self.metrics.take(),
            self.operation.take(),
            self.started.take(),
        ) else {
            return;
        };
        metrics.record_duration(operation, outcome, reason, started.elapsed().as_secs_f64());
    }
}

impl Drop for OperationTimer {
    fn drop(&mut self) {
        self.complete(Outcome::Error, Reason::Internal);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        catalog::{ComponentName, GaugeName},
        recorder::Metrics,
    };

    use super::*;

    #[test]
    fn explicit_finish_records_exactly_once() {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        metrics
            .operation_started(OperationName::CapacityQuery)
            .finish(Outcome::Success, Reason::None);
        let snapshot = metrics.snapshot().unwrap();
        let values: Vec<_> = snapshot.operations().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].1.count(), 1);
        assert_eq!(values[0].0.outcome(), Outcome::Success);
    }

    #[test]
    fn unfinished_drop_records_internal_error_once() {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        drop(metrics.operation_started(OperationName::CapacityQuery));
        let snapshot = metrics.snapshot().unwrap();
        let values: Vec<_> = snapshot.operations().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].1.count(), 1);
        assert_eq!(values[0].0.outcome(), Outcome::Error);
        assert_eq!(values[0].0.reason(), Reason::Internal);
    }

    #[test]
    fn no_op_metrics_have_no_snapshot_or_state() {
        let metrics = Metrics::noop();
        metrics
            .operation_started(OperationName::CapacityQuery)
            .finish(Outcome::Success, Reason::None);
        metrics.record_event(crate::EventName::ReplayRefusal);
        metrics.set_gauge(GaugeName::AttachedAgents, 3);
        assert!(!metrics.is_enabled());
        assert_eq!(metrics.snapshot(), None);
    }

    #[test]
    fn gauge_set_is_absolute_and_idempotent() {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        metrics.set_gauge(GaugeName::AttachedAgents, 3);
        metrics.set_gauge(GaugeName::AttachedAgents, 3);
        let snapshot = metrics.snapshot().unwrap();
        let values: Vec<_> = snapshot.gauges().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(*values[0].1, 3);
    }

    #[test]
    fn illegal_catalog_use_counts_one_failure() {
        let metrics = Metrics::registered(ComponentName::Agent);
        metrics.record_event(crate::EventName::RateLimitRefusal);
        assert_eq!(metrics.snapshot().unwrap().recording_failures(), 1);
    }
}
