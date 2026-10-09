#![forbid(unsafe_code)]

use std::fmt::{self, Write};

use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};

pub const HISTOGRAM_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

pub const OPERATION_FAMILY_NAME: &str = "podmesh_operation_duration_seconds";
pub const OPERATION_FAMILY_HELP: &str = "Completed Podmesh operation duration in seconds";
pub const EVENT_FAMILY_NAME: &str = "podmesh_events";
pub const EVENT_FAMILY_HELP: &str = "Podmesh bounded operational events";
pub const STATE_FAMILY_NAME: &str = "podmesh_state";
pub const STATE_FAMILY_HELP: &str = "Current Podmesh bounded process state";
pub const FAILURE_FAMILY_NAME: &str = "podmesh_metrics_recording_failures";
pub const FAILURE_FAMILY_HELP: &str = "Podmesh internal metrics recording failures";

macro_rules! label_enum {
	(
		$(#[$meta:meta])*
		pub enum $name:ident {
			$($variant:ident => $wire:literal),+ $(,)?
		}
	) => {
		$(#[$meta])*
		#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
		pub enum $name {
			$($variant),+
		}

		impl $name {
			pub const ALL: &'static [Self] = &[$(Self::$variant),+];

			pub const fn as_str(self) -> &'static str {
				match self {
					$(Self::$variant => $wire),+
				}
			}
		}

		impl EncodeLabelValue for $name {
			fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> fmt::Result {
				encoder.write_str(self.as_str())
			}
		}
	};
}

label_enum! {
    pub enum ComponentName {
        Scheduler => "scheduler",
        Agent => "agent",
        Proxy => "proxy",
        Sidecar => "sidecar",
    }
}

label_enum! {
    pub enum OperationGroup {
        Coordination => "coordination",
        Control => "control",
        Traffic => "traffic",
        Lifecycle => "lifecycle",
    }
}

label_enum! {
    pub enum OperationName {
        CapacityQuery => "capacity_query",
        CapacityFanout => "capacity_fanout",
        CapacitySelection => "capacity_selection",
        AgentAttachment => "agent_attachment",
        SchedulerGossip => "scheduler_gossip",
        ControlRelay => "control_relay",
        Reconciliation => "reconciliation",
        ClientRequest => "client_request",
        SchedulerAttachment => "scheduler_attachment",
        Admission => "admission",
        Reservation => "reservation",
        Deploy => "deploy",
        Update => "update",
        Status => "status",
        Logs => "logs",
        Delete => "delete",
        RuntimeRestore => "runtime_restore",
        ProxyGrant => "proxy_grant",
        PeerAnnouncement => "peer_announcement",
        WorkloadHandshake => "workload_handshake",
        Registration => "registration",
        Discovery => "discovery",
        Ingress => "ingress",
        Egress => "egress",
        RouteUpdate => "route_update",
        ProxyConnection => "proxy_connection",
        WebsocketRelay => "websocket_relay",
        MetricsScrape => "metrics_scrape",
    }
}

label_enum! {
    pub enum Outcome {
        Success => "success",
        Refused => "refused",
        Timeout => "timeout",
        Saturated => "saturated",
        Unreachable => "unreachable",
        Error => "error",
    }
}

label_enum! {
    pub enum Reason {
        None => "none",
        Authorization => "authorization",
        Replay => "replay",
        RateLimit => "rate_limit",
        Policy => "policy",
        Invalid => "invalid",
        Bound => "bound",
        Deadline => "deadline",
        Capacity => "capacity",
        Unavailable => "unavailable",
        Internal => "internal",
    }
}

label_enum! {
    pub enum EventName {
        ReplayRefusal => "replay_refusal",
        RateLimitRefusal => "rate_limit_refusal",
        StoreSaturation => "store_saturation",
        StreamSaturation => "stream_saturation",
        FanoutSaturation => "fanout_saturation",
    }
}

label_enum! {
    pub enum GaugeName {
        AttachedAgents => "attached_agents",
        SchedulerPeers => "scheduler_peers",
        PendingCapacityQueries => "pending_capacity_queries",
        ActiveReservations => "active_reservations",
        RunningWorkloads => "running_workloads",
        AttachedSchedulers => "attached_schedulers",
        ProxyPeers => "proxy_peers",
        TenantSessions => "tenant_sessions",
        RouteKeys => "route_keys",
        RouteBackends => "route_backends",
        ActiveIngress => "active_ingress",
        ActiveEgress => "active_egress",
        ActiveWebsockets => "active_websockets",
        ProxySessions => "proxy_sessions",
    }
}

pub const TERMINAL_CLASSIFICATIONS: [(Outcome, Reason); 11] = [
    (Outcome::Success, Reason::None),
    (Outcome::Refused, Reason::Authorization),
    (Outcome::Refused, Reason::Replay),
    (Outcome::Refused, Reason::RateLimit),
    (Outcome::Refused, Reason::Policy),
    (Outcome::Refused, Reason::Invalid),
    (Outcome::Refused, Reason::Bound),
    (Outcome::Timeout, Reason::Deadline),
    (Outcome::Saturated, Reason::Capacity),
    (Outcome::Unreachable, Reason::Unavailable),
    (Outcome::Error, Reason::Internal),
];

pub const OPERATION_PAIRS: [(ComponentName, OperationName); 37] = [
    (ComponentName::Scheduler, OperationName::CapacityQuery),
    (ComponentName::Scheduler, OperationName::CapacityFanout),
    (ComponentName::Scheduler, OperationName::CapacitySelection),
    (ComponentName::Scheduler, OperationName::AgentAttachment),
    (ComponentName::Scheduler, OperationName::SchedulerGossip),
    (ComponentName::Scheduler, OperationName::ControlRelay),
    (ComponentName::Scheduler, OperationName::Reconciliation),
    (ComponentName::Scheduler, OperationName::ClientRequest),
    (ComponentName::Scheduler, OperationName::MetricsScrape),
    (ComponentName::Agent, OperationName::SchedulerAttachment),
    (ComponentName::Agent, OperationName::Admission),
    (ComponentName::Agent, OperationName::Reservation),
    (ComponentName::Agent, OperationName::Deploy),
    (ComponentName::Agent, OperationName::Update),
    (ComponentName::Agent, OperationName::Status),
    (ComponentName::Agent, OperationName::Logs),
    (ComponentName::Agent, OperationName::Delete),
    (ComponentName::Agent, OperationName::Reconciliation),
    (ComponentName::Agent, OperationName::RuntimeRestore),
    (ComponentName::Agent, OperationName::MetricsScrape),
    (ComponentName::Proxy, OperationName::ProxyGrant),
    (ComponentName::Proxy, OperationName::PeerAnnouncement),
    (ComponentName::Proxy, OperationName::WorkloadHandshake),
    (ComponentName::Proxy, OperationName::Registration),
    (ComponentName::Proxy, OperationName::Discovery),
    (ComponentName::Proxy, OperationName::Ingress),
    (ComponentName::Proxy, OperationName::Egress),
    (ComponentName::Proxy, OperationName::RouteUpdate),
    (ComponentName::Proxy, OperationName::MetricsScrape),
    (ComponentName::Sidecar, OperationName::ProxyConnection),
    (ComponentName::Sidecar, OperationName::WorkloadHandshake),
    (ComponentName::Sidecar, OperationName::Registration),
    (ComponentName::Sidecar, OperationName::Discovery),
    (ComponentName::Sidecar, OperationName::Ingress),
    (ComponentName::Sidecar, OperationName::Egress),
    (ComponentName::Sidecar, OperationName::WebsocketRelay),
    (ComponentName::Sidecar, OperationName::MetricsScrape),
];

pub const EVENT_PAIRS: [(ComponentName, EventName); 14] = [
    (ComponentName::Scheduler, EventName::ReplayRefusal),
    (ComponentName::Agent, EventName::ReplayRefusal),
    (ComponentName::Proxy, EventName::ReplayRefusal),
    (ComponentName::Sidecar, EventName::ReplayRefusal),
    (ComponentName::Scheduler, EventName::RateLimitRefusal),
    (ComponentName::Proxy, EventName::RateLimitRefusal),
    (ComponentName::Scheduler, EventName::StoreSaturation),
    (ComponentName::Agent, EventName::StoreSaturation),
    (ComponentName::Proxy, EventName::StoreSaturation),
    (ComponentName::Sidecar, EventName::StoreSaturation),
    (ComponentName::Proxy, EventName::StreamSaturation),
    (ComponentName::Sidecar, EventName::StreamSaturation),
    (ComponentName::Scheduler, EventName::FanoutSaturation),
    (ComponentName::Agent, EventName::FanoutSaturation),
];

pub const GAUGE_PAIRS: [(ComponentName, GaugeName); 17] = [
    (ComponentName::Scheduler, GaugeName::AttachedAgents),
    (ComponentName::Scheduler, GaugeName::SchedulerPeers),
    (ComponentName::Scheduler, GaugeName::PendingCapacityQueries),
    (ComponentName::Agent, GaugeName::ActiveReservations),
    (ComponentName::Agent, GaugeName::RunningWorkloads),
    (ComponentName::Agent, GaugeName::AttachedSchedulers),
    (ComponentName::Proxy, GaugeName::ProxyPeers),
    (ComponentName::Proxy, GaugeName::TenantSessions),
    (ComponentName::Proxy, GaugeName::RouteKeys),
    (ComponentName::Proxy, GaugeName::RouteBackends),
    (ComponentName::Proxy, GaugeName::ActiveIngress),
    (ComponentName::Proxy, GaugeName::ActiveEgress),
    (ComponentName::Proxy, GaugeName::ActiveWebsockets),
    (ComponentName::Sidecar, GaugeName::ProxySessions),
    (ComponentName::Sidecar, GaugeName::ActiveIngress),
    (ComponentName::Sidecar, GaugeName::ActiveEgress),
    (ComponentName::Sidecar, GaugeName::ActiveWebsockets),
];

pub const OPERATION_INSTANCE_COUNT: usize = OPERATION_PAIRS.len() * TERMINAL_CLASSIFICATIONS.len();
pub const EVENT_INSTANCE_COUNT: usize = EVENT_PAIRS.len();
pub const GAUGE_INSTANCE_COUNT: usize = GAUGE_PAIRS.len();
pub const FAILURE_INSTANCE_COUNT: usize = ComponentName::ALL.len();
pub const MAX_LABELED_INSTANCES: usize =
    OPERATION_INSTANCE_COUNT + EVENT_INSTANCE_COUNT + GAUGE_INSTANCE_COUNT + FAILURE_INSTANCE_COUNT;
pub const HISTOGRAM_SAMPLES_PER_INSTANCE: usize = HISTOGRAM_BUCKETS.len() + 3;
pub const MAX_RENDERED_SAMPLES: usize = OPERATION_INSTANCE_COUNT * HISTOGRAM_SAMPLES_PER_INSTANCE
    + EVENT_INSTANCE_COUNT
    + GAUGE_INSTANCE_COUNT
    + FAILURE_INSTANCE_COUNT;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogError {
    OperationNotOwned,
    InvalidTerminalClassification,
    EventNotProduced,
    GaugeNotOwned,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OperationNotOwned => "operation is not owned by component",
            Self::InvalidTerminalClassification => "invalid outcome and reason pair",
            Self::EventNotProduced => "event is not produced by component",
            Self::GaugeNotOwned => "gauge is not owned by component",
        })
    }
}

impl std::error::Error for CatalogError {}

pub const fn operation_group(operation: OperationName) -> OperationGroup {
    match operation {
        OperationName::CapacityQuery
        | OperationName::CapacityFanout
        | OperationName::CapacitySelection
        | OperationName::AgentAttachment
        | OperationName::SchedulerGossip
        | OperationName::SchedulerAttachment
        | OperationName::PeerAnnouncement
        | OperationName::Discovery
        | OperationName::ProxyConnection => OperationGroup::Coordination,
        OperationName::ControlRelay
        | OperationName::Reconciliation
        | OperationName::ClientRequest
        | OperationName::Admission
        | OperationName::Status
        | OperationName::Logs
        | OperationName::ProxyGrant
        | OperationName::WorkloadHandshake
        | OperationName::Registration
        | OperationName::MetricsScrape => OperationGroup::Control,
        OperationName::Ingress | OperationName::Egress | OperationName::WebsocketRelay => {
            OperationGroup::Traffic
        }
        OperationName::Reservation
        | OperationName::Deploy
        | OperationName::Update
        | OperationName::Delete
        | OperationName::RuntimeRestore
        | OperationName::RouteUpdate => OperationGroup::Lifecycle,
    }
}

pub fn validate_operation(
    component: ComponentName,
    operation: OperationName,
) -> Result<(), CatalogError> {
    OPERATION_PAIRS
        .contains(&(component, operation))
        .then_some(())
        .ok_or(CatalogError::OperationNotOwned)
}

pub fn validate_terminal(outcome: Outcome, reason: Reason) -> Result<(), CatalogError> {
    TERMINAL_CLASSIFICATIONS
        .contains(&(outcome, reason))
        .then_some(())
        .ok_or(CatalogError::InvalidTerminalClassification)
}

pub fn validate_event(component: ComponentName, event: EventName) -> Result<(), CatalogError> {
    EVENT_PAIRS
        .contains(&(component, event))
        .then_some(())
        .ok_or(CatalogError::EventNotProduced)
}

pub fn validate_gauge(component: ComponentName, gauge: GaugeName) -> Result<(), CatalogError> {
    GAUGE_PAIRS
        .contains(&(component, gauge))
        .then_some(())
        .ok_or(CatalogError::GaugeNotOwned)
}

pub const fn event_classification(event: EventName) -> (Outcome, Reason) {
    match event {
        EventName::ReplayRefusal => (Outcome::Refused, Reason::Replay),
        EventName::RateLimitRefusal => (Outcome::Refused, Reason::RateLimit),
        EventName::StoreSaturation | EventName::StreamSaturation | EventName::FanoutSaturation => {
            (Outcome::Saturated, Reason::Capacity)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationKey {
    component: ComponentName,
    operation: OperationName,
    group: OperationGroup,
    outcome: Outcome,
    reason: Reason,
}

impl OperationKey {
    pub fn new(
        component: ComponentName,
        operation: OperationName,
        outcome: Outcome,
        reason: Reason,
    ) -> Result<Self, CatalogError> {
        validate_operation(component, operation)?;
        validate_terminal(outcome, reason)?;
        Ok(Self {
            component,
            operation,
            group: operation_group(operation),
            outcome,
            reason,
        })
    }

    pub const fn component(self) -> ComponentName {
        self.component
    }

    pub const fn operation(self) -> OperationName {
        self.operation
    }

    pub const fn group(self) -> OperationGroup {
        self.group
    }

    pub const fn outcome(self) -> Outcome {
        self.outcome
    }

    pub const fn reason(self) -> Reason {
        self.reason
    }

    pub const fn labels(self) -> OperationLabels {
        OperationLabels {
            component: self.component,
            group: self.group,
            operation: self.operation,
            outcome: self.outcome,
            reason: self.reason,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventKey {
    component: ComponentName,
    event: EventName,
    outcome: Outcome,
    reason: Reason,
}

impl EventKey {
    pub fn new(component: ComponentName, event: EventName) -> Result<Self, CatalogError> {
        validate_event(component, event)?;
        let (outcome, reason) = event_classification(event);
        Ok(Self {
            component,
            event,
            outcome,
            reason,
        })
    }

    pub const fn component(self) -> ComponentName {
        self.component
    }

    pub const fn event(self) -> EventName {
        self.event
    }

    pub const fn outcome(self) -> Outcome {
        self.outcome
    }

    pub const fn reason(self) -> Reason {
        self.reason
    }

    pub const fn labels(self) -> EventLabels {
        EventLabels {
            component: self.component,
            event: self.event,
            outcome: self.outcome,
            reason: self.reason,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GaugeKey {
    component: ComponentName,
    gauge: GaugeName,
}

impl GaugeKey {
    pub fn new(component: ComponentName, gauge: GaugeName) -> Result<Self, CatalogError> {
        validate_gauge(component, gauge)?;
        Ok(Self { component, gauge })
    }

    pub const fn component(self) -> ComponentName {
        self.component
    }

    pub const fn gauge(self) -> GaugeName {
        self.gauge
    }

    pub const fn labels(self) -> StateLabels {
        StateLabels {
            component: self.component,
            gauge: self.gauge,
        }
    }
}

#[derive(Clone, Copy, Debug, EncodeLabelSet)]
pub struct OperationLabels {
    pub component: ComponentName,
    pub group: OperationGroup,
    pub operation: OperationName,
    pub outcome: Outcome,
    pub reason: Reason,
}

#[derive(Clone, Copy, Debug, EncodeLabelSet)]
pub struct EventLabels {
    pub component: ComponentName,
    pub event: EventName,
    pub outcome: Outcome,
    pub reason: Reason,
}

#[derive(Clone, Copy, Debug, EncodeLabelSet)]
pub struct StateLabels {
    pub component: ComponentName,
    pub gauge: GaugeName,
}

#[derive(Clone, Copy, Debug, EncodeLabelSet)]
pub struct FailureLabels {
    pub component: ComponentName,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn assert_unique<T: Copy + Ord + fmt::Debug>(values: &[T]) {
        let unique: BTreeSet<_> = values.iter().copied().collect();
        assert_eq!(unique.len(), values.len(), "duplicate value in {values:?}");
    }

    #[test]
    fn catalog_cardinality_matches_the_approved_budget() {
        assert_eq!(OPERATION_PAIRS.len(), 37);
        assert_eq!(TERMINAL_CLASSIFICATIONS.len(), 11);
        assert_eq!(OPERATION_INSTANCE_COUNT, 407);
        assert_eq!(EVENT_PAIRS.len(), 14);
        assert_eq!(GAUGE_PAIRS.len(), 17);
        assert_eq!(FAILURE_INSTANCE_COUNT, 4);
        assert_eq!(MAX_LABELED_INSTANCES, 442);
        assert_eq!(HISTOGRAM_SAMPLES_PER_INSTANCE, 14);
        assert_eq!(MAX_RENDERED_SAMPLES, 5_733);
    }

    #[test]
    fn all_catalog_dimensions_and_pairs_are_unique() {
        assert_unique(ComponentName::ALL);
        assert_unique(OperationGroup::ALL);
        assert_unique(OperationName::ALL);
        assert_unique(Outcome::ALL);
        assert_unique(Reason::ALL);
        assert_unique(EventName::ALL);
        assert_unique(GaugeName::ALL);
        assert_unique(&OPERATION_PAIRS);
        assert_unique(&TERMINAL_CLASSIFICATIONS);
        assert_unique(&EVENT_PAIRS);
        assert_unique(&GAUGE_PAIRS);
    }

    #[test]
    fn every_operation_has_one_group_and_every_pair_constructs() {
        for operation in OperationName::ALL {
            assert!(OperationGroup::ALL.contains(&operation_group(*operation)));
        }
        for (component, operation) in OPERATION_PAIRS {
            for (outcome, reason) in TERMINAL_CLASSIFICATIONS {
                let key = OperationKey::new(component, operation, outcome, reason).unwrap();
                assert_eq!(key.group(), operation_group(operation));
            }
        }
    }

    #[test]
    fn illegal_component_pairs_and_terminal_pairs_are_refused() {
        assert_eq!(
            OperationKey::new(
                ComponentName::Agent,
                OperationName::CapacityQuery,
                Outcome::Success,
                Reason::None,
            ),
            Err(CatalogError::OperationNotOwned)
        );
        assert_eq!(
            OperationKey::new(
                ComponentName::Scheduler,
                OperationName::CapacityQuery,
                Outcome::Success,
                Reason::Internal,
            ),
            Err(CatalogError::InvalidTerminalClassification)
        );
        assert_eq!(
            EventKey::new(ComponentName::Agent, EventName::RateLimitRefusal),
            Err(CatalogError::EventNotProduced)
        );
        assert_eq!(
            GaugeKey::new(ComponentName::Sidecar, GaugeName::RouteKeys),
            Err(CatalogError::GaugeNotOwned)
        );
    }

    #[test]
    fn event_classification_is_fixed_by_the_event() {
        for (component, event) in EVENT_PAIRS {
            let key = EventKey::new(component, event).unwrap();
            assert_eq!((key.outcome(), key.reason()), event_classification(event));
        }
    }

    #[test]
    fn wire_names_are_stable_snake_case() {
        assert_eq!(ComponentName::Scheduler.as_str(), "scheduler");
        assert_eq!(OperationName::CapacityQuery.as_str(), "capacity_query");
        assert_eq!(OperationName::WebsocketRelay.as_str(), "websocket_relay");
        assert_eq!(Reason::RateLimit.as_str(), "rate_limit");
        assert_eq!(EventName::StoreSaturation.as_str(), "store_saturation");
        assert_eq!(GaugeName::ActiveWebsockets.as_str(), "active_websockets");

        for wire_name in ComponentName::ALL
            .iter()
            .map(|value| value.as_str())
            .chain(OperationGroup::ALL.iter().map(|value| value.as_str()))
            .chain(OperationName::ALL.iter().map(|value| value.as_str()))
            .chain(Outcome::ALL.iter().map(|value| value.as_str()))
            .chain(Reason::ALL.iter().map(|value| value.as_str()))
            .chain(EventName::ALL.iter().map(|value| value.as_str()))
            .chain(GaugeName::ALL.iter().map(|value| value.as_str()))
        {
            assert!(!wire_name.is_empty());
            assert!(
                wire_name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            );
        }
    }

    #[test]
    fn histogram_buckets_are_positive_finite_and_strictly_increasing() {
        assert!(
            HISTOGRAM_BUCKETS
                .iter()
                .all(|bucket| bucket.is_finite() && *bucket > 0.0)
        );
        assert!(HISTOGRAM_BUCKETS.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(HISTOGRAM_BUCKETS[0], 0.005);
        assert_eq!(HISTOGRAM_BUCKETS[10], 10.0);
    }
}
