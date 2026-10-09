#![forbid(unsafe_code)]

use std::{fmt, sync::Arc};

use prometheus_client::{
    encoding::{EncodeMetric, MetricEncoder, NoLabelSet},
    metrics::{MetricType, TypedMetric},
};

use crate::{catalog::HISTOGRAM_BUCKETS, snapshot::MetricsSnapshot};

#[derive(Clone, Debug)]
pub(crate) struct OperationSnapshotFamily(pub(crate) Arc<MetricsSnapshot>);

impl TypedMetric for OperationSnapshotFamily {
    const TYPE: MetricType = MetricType::Histogram;
}

impl EncodeMetric for OperationSnapshotFamily {
    fn encode(&self, mut encoder: MetricEncoder<'_>) -> fmt::Result {
        for (key, value) in self.0.operations() {
            let labels = key.labels();
            let mut family = encoder.encode_family(&labels)?;
            let buckets: Vec<_> = HISTOGRAM_BUCKETS
                .iter()
                .copied()
                .zip(value.buckets().iter().copied())
                .collect();
            family.encode_histogram::<NoLabelSet>(value.sum(), value.count(), &buckets, None)?;
        }
        Ok(())
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }

    fn is_empty(&self) -> bool {
        self.0.operations().len() == 0
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EventSnapshotFamily(pub(crate) Arc<MetricsSnapshot>);

impl TypedMetric for EventSnapshotFamily {
    const TYPE: MetricType = MetricType::Counter;
}

impl EncodeMetric for EventSnapshotFamily {
    fn encode(&self, mut encoder: MetricEncoder<'_>) -> fmt::Result {
        for (key, value) in self.0.events() {
            let labels = key.labels();
            let mut family = encoder.encode_family(&labels)?;
            family.encode_counter::<NoLabelSet, _, f64>(value, None)?;
        }
        Ok(())
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }

    fn is_empty(&self) -> bool {
        self.0.events().len() == 0
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StateSnapshotFamily(pub(crate) Arc<MetricsSnapshot>);

impl TypedMetric for StateSnapshotFamily {
    const TYPE: MetricType = MetricType::Gauge;
}

impl EncodeMetric for StateSnapshotFamily {
    fn encode(&self, mut encoder: MetricEncoder<'_>) -> fmt::Result {
        for (key, value) in self.0.gauges() {
            let labels = key.labels();
            encoder.encode_family(&labels)?.encode_gauge(value)?;
        }
        Ok(())
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }

    fn is_empty(&self) -> bool {
        self.0.gauges().len() == 0
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FailureSnapshotFamily(pub(crate) Arc<MetricsSnapshot>);

impl TypedMetric for FailureSnapshotFamily {
    const TYPE: MetricType = MetricType::Counter;
}

impl EncodeMetric for FailureSnapshotFamily {
    fn encode(&self, mut encoder: MetricEncoder<'_>) -> fmt::Result {
        let labels = crate::catalog::FailureLabels {
            component: self.0.component(),
        };
        encoder
            .encode_family(&labels)?
            .encode_counter::<NoLabelSet, _, f64>(&self.0.recording_failures(), None)
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }
}
