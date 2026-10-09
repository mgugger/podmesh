mod families;

use std::{fmt, sync::Arc, time::Instant};

use families::{
    EventSnapshotFamily, FailureSnapshotFamily, OperationSnapshotFamily, StateSnapshotFamily,
};
use prometheus_client::{encoding::text, registry::Registry};

use crate::{
    catalog::{
        EVENT_FAMILY_HELP, EVENT_FAMILY_NAME, FAILURE_FAMILY_HELP, FAILURE_FAMILY_NAME,
        OPERATION_FAMILY_HELP, OPERATION_FAMILY_NAME, STATE_FAMILY_HELP, STATE_FAMILY_NAME,
    },
    snapshot::MetricsSnapshot,
};

pub const OPENMETRICS_CONTENT_TYPE: &str =
    "application/openmetrics-text; version=1.0.0; charset=utf-8";
pub const MAX_RENDERED_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedMetricsDocument(String);

impl RenderedMetricsDocument {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    Encoding,
    SizeLimit,
    Deadline,
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Encoding => "metrics encoding failed",
            Self::SizeLimit => "metrics document exceeds size limit",
            Self::Deadline => "metrics rendering exceeded deadline",
        })
    }
}

impl std::error::Error for RenderError {}

pub fn render_snapshot(
    snapshot: &MetricsSnapshot,
    deadline: Instant,
) -> Result<RenderedMetricsDocument, RenderError> {
    if Instant::now() >= deadline {
        return Err(RenderError::Deadline);
    }

    let snapshot = Arc::new(snapshot.clone());
    let mut registry = Registry::default();
    registry.register(
        OPERATION_FAMILY_NAME,
        OPERATION_FAMILY_HELP,
        OperationSnapshotFamily(snapshot.clone()),
    );
    registry.register(
        EVENT_FAMILY_NAME,
        EVENT_FAMILY_HELP,
        EventSnapshotFamily(snapshot.clone()),
    );
    registry.register(
        STATE_FAMILY_NAME,
        STATE_FAMILY_HELP,
        StateSnapshotFamily(snapshot.clone()),
    );
    registry.register(
        FAILURE_FAMILY_NAME,
        FAILURE_FAMILY_HELP,
        FailureSnapshotFamily(snapshot),
    );

    let mut writer = BoundedWriter::new(MAX_RENDERED_BYTES);
    text::encode(&mut writer, &registry).map_err(|_| {
        if writer.exceeded {
            RenderError::SizeLimit
        } else {
            RenderError::Encoding
        }
    })?;
    if Instant::now() >= deadline {
        return Err(RenderError::Deadline);
    }
    if !writer.output.ends_with("# EOF\n") {
        return Err(RenderError::Encoding);
    }
    Ok(RenderedMetricsDocument(writer.output))
}

#[derive(Debug)]
struct BoundedWriter {
    output: String,
    max_bytes: usize,
    exceeded: bool,
}

impl BoundedWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            output: String::new(),
            max_bytes,
            exceeded: false,
        }
    }
}

impl fmt::Write for BoundedWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self
            .output
            .len()
            .checked_add(value.len())
            .is_none_or(|length| length > self.max_bytes)
        {
            self.exceeded = true;
            return Err(fmt::Error);
        }
        self.output.push_str(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{ComponentName, EventName, GaugeName, Metrics, OperationName, Outcome, Reason};

    use super::*;

    fn populated_snapshot() -> MetricsSnapshot {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        metrics
            .operation_started(OperationName::CapacityQuery)
            .finish(Outcome::Success, Reason::None);
        metrics.record_event(EventName::RateLimitRefusal);
        metrics.set_gauge(GaugeName::AttachedAgents, 3);
        metrics.snapshot().unwrap()
    }

    #[test]
    fn openmetrics_document_parses_and_ends_with_eof() {
        let document = render_snapshot(
            &populated_snapshot(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let (remaining, families) =
            nom_openmetrics::parser::openmetrics(document.as_str()).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(families.len(), 4);
        assert!(document.as_str().ends_with("# EOF\n"));
        assert!(document.len() <= MAX_RENDERED_BYTES);
    }

    #[test]
    fn rendering_the_same_snapshot_is_byte_identical() {
        let snapshot = populated_snapshot();
        let deadline = || Instant::now() + Duration::from_secs(1);
        let first = render_snapshot(&snapshot, deadline()).unwrap();
        let second = render_snapshot(&snapshot, deadline()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rendered_names_labels_and_values_are_exact() {
        let document = render_snapshot(
            &populated_snapshot(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let text = document.as_str();
        assert!(text.contains("# TYPE podmesh_operation_duration_seconds histogram"));
        assert!(text.contains("component=\"scheduler\""));
        assert!(text.contains("operation=\"capacity_query\""));
        assert!(text.contains("podmesh_events_total"));
        assert!(text.contains("event=\"rate_limit_refusal\""));
        assert!(
            text.contains("podmesh_state{component=\"scheduler\",gauge=\"attached_agents\"} 3")
        );
        assert!(text.contains("podmesh_metrics_recording_failures_total"));
    }

    #[test]
    fn empty_registered_snapshot_contains_only_failure_counter() {
        let metrics = Metrics::registered(ComponentName::Proxy);
        let document = render_snapshot(
            &metrics.snapshot().unwrap(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert!(
            document
                .as_str()
                .contains("podmesh_metrics_recording_failures_total{component=\"proxy\"} 0")
        );
        assert!(!document.as_str().contains(OPERATION_FAMILY_NAME));
    }

    #[test]
    fn expired_deadline_refuses_rendering() {
        assert_eq!(
            render_snapshot(&populated_snapshot(), Instant::now()),
            Err(RenderError::Deadline)
        );
    }

    #[test]
    fn bounded_writer_refuses_overflow_without_appending() {
        use std::fmt::Write as _;

        let mut writer = BoundedWriter::new(4);
        writer.write_str("1234").unwrap();
        assert!(writer.write_str("5").is_err());
        assert_eq!(writer.output, "1234");
        assert!(writer.exceeded);
    }
}
