#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, btree_map::Entry},
    time::Instant,
};

use parking_lot::Mutex;

use crate::{
    catalog::{ComponentName, EventKey, GaugeKey, OperationKey},
    diagnostics::{DiagnosticAction, DiagnosticSuppressor, FailureCategory},
    snapshot::{HistogramValue, MetricsSnapshot},
};

#[derive(Debug, Default)]
struct RegistryState {
    operations: BTreeMap<OperationKey, HistogramValue>,
    events: BTreeMap<EventKey, u64>,
    gauges: BTreeMap<GaugeKey, u64>,
    recording_failures: u64,
    diagnostics: DiagnosticSuppressor,
}

#[derive(Debug)]
pub(crate) struct RegistryCore {
    component: ComponentName,
    state: Mutex<RegistryState>,
}

impl RegistryCore {
    pub(crate) fn new(component: ComponentName) -> Self {
        Self {
            component,
            state: Mutex::new(RegistryState::default()),
        }
    }

    pub(crate) fn record_duration(&self, key: OperationKey, seconds: f64) {
        let action = {
            let mut state = self.state.lock();
            let mut value = state.operations.get(&key).cloned().unwrap_or_default();
            if value.checked_observe(seconds) {
                state.operations.insert(key, value);
                DiagnosticAction::Silent
            } else {
                record_failure(&mut state, FailureCategory::InvalidValue, Instant::now())
            }
        };
        log_action(self.component, action);
    }

    pub(crate) fn record_event(&self, key: EventKey) {
        let action = {
            let mut state = self.state.lock();
            match state.events.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(1);
                    DiagnosticAction::Silent
                }
                Entry::Occupied(mut entry) if *entry.get() < u64::MAX => {
                    *entry.get_mut() += 1;
                    DiagnosticAction::Silent
                }
                Entry::Occupied(_) => {
                    record_failure(&mut state, FailureCategory::NumericOverflow, Instant::now())
                }
            }
        };
        log_action(self.component, action);
    }

    pub(crate) fn set_gauge(&self, key: GaugeKey, value: u64) {
        self.state.lock().gauges.insert(key, value);
    }

    pub(crate) fn record_failure(&self, category: FailureCategory) {
        let action = {
            let mut state = self.state.lock();
            record_failure(&mut state, category, Instant::now())
        };
        log_action(self.component, action);
    }

    pub(crate) fn snapshot(&self) -> MetricsSnapshot {
        let state = self.state.lock();
        MetricsSnapshot::new(
            self.component,
            state.operations.clone(),
            state.events.clone(),
            state.gauges.clone(),
            state.recording_failures,
        )
    }
}

fn record_failure(
    state: &mut RegistryState,
    category: FailureCategory,
    now: Instant,
) -> DiagnosticAction {
    state.recording_failures = state.recording_failures.saturating_add(1);
    state.diagnostics.observe(category, now)
}

fn log_action(component: ComponentName, action: DiagnosticAction) {
    match action {
        DiagnosticAction::Silent => {}
        DiagnosticAction::LogFirst { category } => log::warn!(
            "metrics recording failure component={} category={}",
            component.as_str(),
            category.as_str()
        ),
        DiagnosticAction::LogSummary {
            category,
            suppressed_count,
        } => log::warn!(
            "metrics recording failures component={} category={} suppressed_count={}",
            component.as_str(),
            category.as_str(),
            suppressed_count
        ),
    }
}
