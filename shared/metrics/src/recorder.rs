#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::{
    catalog::{
        ComponentName, EventKey, EventName, GaugeKey, GaugeName, OperationKey, OperationName,
        Outcome, Reason,
    },
    diagnostics::FailureCategory,
    registry::RegistryCore,
    snapshot::MetricsSnapshot,
    timer::OperationTimer,
};

pub(crate) trait MetricsRecorder: Send + Sync {
    fn is_enabled(&self) -> bool;
    fn component(&self) -> Option<ComponentName>;
    fn record_duration(
        &self,
        operation: OperationName,
        outcome: Outcome,
        reason: Reason,
        seconds: f64,
    );
    fn record_event(&self, event: EventName);
    fn set_gauge(&self, gauge: GaugeName, value: u64);
    fn snapshot(&self) -> Option<MetricsSnapshot>;
}

#[derive(Clone)]
pub struct Metrics {
    recorder: Arc<dyn MetricsRecorder>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Metrics")
            .field("component", &self.component())
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::noop()
    }
}

impl Metrics {
    pub fn noop() -> Self {
        Self {
            recorder: Arc::new(NoopMetrics),
        }
    }

    pub fn registered(component: ComponentName) -> Self {
        Self {
            recorder: Arc::new(RegisteredMetrics {
                component,
                registry: Arc::new(RegistryCore::new(component)),
            }),
        }
    }

    pub fn operation_started(&self, operation: OperationName) -> OperationTimer {
        if self.recorder.is_enabled() {
            OperationTimer::registered(self.clone(), operation)
        } else {
            OperationTimer::noop()
        }
    }

    pub fn record_event(&self, event: EventName) {
        self.recorder.record_event(event);
    }

    pub fn set_gauge(&self, gauge: GaugeName, value: u64) {
        self.recorder.set_gauge(gauge, value);
    }

    pub fn snapshot(&self) -> Option<MetricsSnapshot> {
        self.recorder.snapshot()
    }

    pub fn is_enabled(&self) -> bool {
        self.recorder.is_enabled()
    }

    pub fn component(&self) -> Option<ComponentName> {
        self.recorder.component()
    }

    pub(crate) fn record_duration(
        &self,
        operation: OperationName,
        outcome: Outcome,
        reason: Reason,
        seconds: f64,
    ) {
        self.recorder
            .record_duration(operation, outcome, reason, seconds);
    }
}

#[derive(Debug)]
struct NoopMetrics;

impl MetricsRecorder for NoopMetrics {
    fn is_enabled(&self) -> bool {
        false
    }

    fn component(&self) -> Option<ComponentName> {
        None
    }

    fn record_duration(
        &self,
        _operation: OperationName,
        _outcome: Outcome,
        _reason: Reason,
        _seconds: f64,
    ) {
    }

    fn record_event(&self, _event: EventName) {}

    fn set_gauge(&self, _gauge: GaugeName, _value: u64) {}

    fn snapshot(&self) -> Option<MetricsSnapshot> {
        None
    }
}

#[derive(Debug)]
struct RegisteredMetrics {
    component: ComponentName,
    registry: Arc<RegistryCore>,
}

impl MetricsRecorder for RegisteredMetrics {
    fn is_enabled(&self) -> bool {
        true
    }

    fn component(&self) -> Option<ComponentName> {
        Some(self.component)
    }

    fn record_duration(
        &self,
        operation: OperationName,
        outcome: Outcome,
        reason: Reason,
        seconds: f64,
    ) {
        match OperationKey::new(self.component, operation, outcome, reason) {
            Ok(key) => self.registry.record_duration(key, seconds),
            Err(_) => self
                .registry
                .record_failure(FailureCategory::InvalidCatalog),
        }
    }

    fn record_event(&self, event: EventName) {
        match EventKey::new(self.component, event) {
            Ok(key) => self.registry.record_event(key),
            Err(_) => self
                .registry
                .record_failure(FailureCategory::InvalidCatalog),
        }
    }

    fn set_gauge(&self, gauge: GaugeName, value: u64) {
        match GaugeKey::new(self.component, gauge) {
            Ok(key) => self.registry.set_gauge(key, value),
            Err(_) => self
                .registry
                .record_failure(FailureCategory::InvalidCatalog),
        }
    }

    fn snapshot(&self) -> Option<MetricsSnapshot> {
        Some(self.registry.snapshot())
    }
}
