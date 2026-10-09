#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use crate::catalog::{ComponentName, EventKey, GaugeKey, HISTOGRAM_BUCKETS, OperationKey};

#[derive(Clone, Debug, PartialEq)]
pub struct HistogramValue {
    buckets: [u64; HISTOGRAM_BUCKETS.len()],
    count: u64,
    sum: f64,
}

impl Default for HistogramValue {
    fn default() -> Self {
        Self {
            buckets: [0; HISTOGRAM_BUCKETS.len()],
            count: 0,
            sum: 0.0,
        }
    }
}

impl HistogramValue {
    pub(crate) fn checked_observe(&mut self, seconds: f64) -> bool {
        if !seconds.is_finite() || seconds < 0.0 || !self.sum.is_finite() {
            return false;
        }
        let sum = self.sum + seconds;
        if !sum.is_finite() || self.count == u64::MAX {
            return false;
        }
        if self
            .buckets
            .iter()
            .zip(HISTOGRAM_BUCKETS)
            .any(|(count, bound)| seconds <= bound && *count == u64::MAX)
        {
            return false;
        }

        self.count += 1;
        self.sum = sum;
        for (count, bound) in self.buckets.iter_mut().zip(HISTOGRAM_BUCKETS) {
            if seconds <= bound {
                *count += 1;
            }
        }
        true
    }

    pub const fn buckets(&self) -> &[u64; HISTOGRAM_BUCKETS.len()] {
        &self.buckets
    }

    pub const fn count(&self) -> u64 {
        self.count
    }

    pub const fn sum(&self) -> f64 {
        self.sum
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetricsSnapshot {
    component: ComponentName,
    operations: BTreeMap<OperationKey, HistogramValue>,
    events: BTreeMap<EventKey, u64>,
    gauges: BTreeMap<GaugeKey, u64>,
    recording_failures: u64,
}

impl MetricsSnapshot {
    pub(crate) fn new(
        component: ComponentName,
        operations: BTreeMap<OperationKey, HistogramValue>,
        events: BTreeMap<EventKey, u64>,
        gauges: BTreeMap<GaugeKey, u64>,
        recording_failures: u64,
    ) -> Self {
        Self {
            component,
            operations,
            events,
            gauges,
            recording_failures,
        }
    }

    pub const fn component(&self) -> ComponentName {
        self.component
    }

    pub fn operations(&self) -> impl ExactSizeIterator<Item = (&OperationKey, &HistogramValue)> {
        self.operations.iter()
    }

    pub fn events(&self) -> impl ExactSizeIterator<Item = (&EventKey, &u64)> {
        self.events.iter()
    }

    pub fn gauges(&self) -> impl ExactSizeIterator<Item = (&GaugeKey, &u64)> {
        self.gauges.iter()
    }

    pub const fn recording_failures(&self) -> u64 {
        self.recording_failures
    }
}
