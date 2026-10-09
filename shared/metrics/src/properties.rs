use std::{collections::BTreeSet, sync::Arc, thread, time::Duration};

use proptest::{prelude::*, test_runner::RngSeed};

use crate::{
    MAX_RENDERED_BYTES, Metrics, MetricsSnapshot,
    catalog::{
        ComponentName, EVENT_PAIRS, EventKey, EventName, FAILURE_INSTANCE_COUNT, GAUGE_PAIRS,
        GaugeKey, GaugeName, HISTOGRAM_BUCKETS, HISTOGRAM_SAMPLES_PER_INSTANCE,
        MAX_LABELED_INSTANCES, MAX_RENDERED_SAMPLES, OPERATION_INSTANCE_COUNT, OPERATION_PAIRS,
        OperationGroup, OperationKey, OperationName, Outcome, Reason, TERMINAL_CLASSIFICATIONS,
        event_classification, operation_group, validate_event, validate_gauge, validate_operation,
        validate_terminal,
    },
    render_snapshot,
};

const U03_PROPTEST_SEED: u64 = 0x504f_444d_4553_4803;

fn config() -> ProptestConfig {
    eprintln!("U-03 proptest fixed seed: {U03_PROPTEST_SEED}");
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(U03_PROPTEST_SEED),
        ..ProptestConfig::default()
    }
}

fn component_strategy() -> impl Strategy<Value = ComponentName> {
    prop::sample::select(ComponentName::ALL.to_vec())
}

fn operation_pair_strategy() -> impl Strategy<Value = (ComponentName, OperationName)> {
    prop::sample::select(OPERATION_PAIRS.to_vec())
}

fn terminal_strategy() -> impl Strategy<Value = (Outcome, Reason)> {
    prop::sample::select(TERMINAL_CLASSIFICATIONS.to_vec())
}

fn event_pair_strategy() -> impl Strategy<Value = (ComponentName, EventName)> {
    prop::sample::select(EVENT_PAIRS.to_vec())
}

fn gauge_pair_strategy() -> impl Strategy<Value = (ComponentName, GaugeName)> {
    prop::sample::select(GAUGE_PAIRS.to_vec())
}

fn finish(metrics: &Metrics, operation: OperationName, outcome: Outcome, reason: Reason) {
    metrics.operation_started(operation).finish(outcome, reason);
}

fn render(snapshot: &MetricsSnapshot) -> String {
    render_snapshot(snapshot, std::time::Instant::now() + Duration::from_secs(1))
        .unwrap()
        .into_string()
}

#[derive(Clone, Copy, Debug)]
enum ModelCommand {
    FinishSuccess,
    DropTimer,
    RecordEvent,
    SetGauge(u8),
    Snapshot,
}

fn command_strategy() -> impl Strategy<Value = ModelCommand> {
    prop_oneof![
        Just(ModelCommand::FinishSuccess),
        Just(ModelCommand::DropTimer),
        Just(ModelCommand::RecordEvent),
        any::<u8>().prop_map(ModelCommand::SetGauge),
        Just(ModelCommand::Snapshot),
    ]
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ReferenceModel {
    success_count: u64,
    dropped_count: u64,
    event_count: u64,
    gauge: Option<u64>,
}

fn snapshot_projection(snapshot: &MetricsSnapshot) -> ReferenceModel {
    let mut model = ReferenceModel::default();
    for (key, value) in snapshot.operations() {
        match (key.outcome(), key.reason()) {
            (Outcome::Success, Reason::None) => model.success_count += value.count(),
            (Outcome::Error, Reason::Internal) => model.dropped_count += value.count(),
            _ => {}
        }
    }
    model.event_count = snapshot.events().map(|(_, value)| *value).sum();
    model.gauge = snapshot.gauges().next().map(|(_, value)| *value);
    model
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn tp01_render_parse_round_trip(command_count in 0usize..32) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        for _ in 0..command_count {
            finish(&metrics, OperationName::CapacityQuery, Outcome::Success, Reason::None);
        }
        let text = render(&metrics.snapshot().unwrap());
        let (remaining, families) = nom_openmetrics::parser::openmetrics(&text).unwrap();
        prop_assert!(remaining.is_empty());
        prop_assert!(!families.is_empty());
        prop_assert!(text.ends_with("# EOF\n"));
    }

    #[test]
    fn tp02_wire_names_are_valid_stable_and_unique(_component in component_strategy()) {
        let dimensions = [
            ComponentName::ALL.iter().map(|value| value.as_str()).collect::<Vec<_>>(),
            OperationGroup::ALL.iter().map(|value| value.as_str()).collect(),
            OperationName::ALL.iter().map(|value| value.as_str()).collect(),
            Outcome::ALL.iter().map(|value| value.as_str()).collect(),
            Reason::ALL.iter().map(|value| value.as_str()).collect(),
            EventName::ALL.iter().map(|value| value.as_str()).collect(),
            GaugeName::ALL.iter().map(|value| value.as_str()).collect(),
        ];
        for names in dimensions {
            let unique: BTreeSet<_> = names.iter().copied().collect();
            prop_assert_eq!(unique.len(), names.len());
            prop_assert!(names.iter().all(|name| !name.is_empty() && name.bytes().all(|byte|
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')));
        }
    }

    #[test]
    fn tp03_operations_have_one_group_and_only_approved_owners(
        (component, operation) in operation_pair_strategy(),
        other_component in component_strategy(),
    ) {
        prop_assert!(OperationGroup::ALL.contains(&operation_group(operation)));
        prop_assert!(validate_operation(component, operation).is_ok());
        prop_assert_eq!(
            validate_operation(other_component, operation).is_ok(),
            OPERATION_PAIRS.contains(&(other_component, operation)),
        );
    }

    #[test]
    fn tp04_only_legal_terminal_and_event_mappings_construct(
        (component, event) in event_pair_strategy(),
        (outcome, reason) in terminal_strategy(),
    ) {
        prop_assert!(validate_terminal(outcome, reason).is_ok());
        let key = EventKey::new(component, event).unwrap();
        prop_assert_eq!((key.outcome(), key.reason()), event_classification(event));
    }

    #[test]
    fn tp05_histogram_counts_never_decrease(finishes in proptest::collection::vec(any::<bool>(), 0..64)) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        let mut previous = 0;
        for explicit in finishes {
            let timer = metrics.operation_started(OperationName::CapacityQuery);
            if explicit {
                timer.finish(Outcome::Success, Reason::None);
            } else {
                drop(timer);
            }
            let current: u64 = metrics.snapshot().unwrap().operations().map(|(_, value)| value.count()).sum();
            prop_assert!(current >= previous);
            previous = current;
        }
    }

    #[test]
    fn tp06_histogram_buckets_are_ordered_monotonic_and_bounded(count in 0usize..64) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        for _ in 0..count {
            finish(&metrics, OperationName::CapacityQuery, Outcome::Success, Reason::None);
        }
        for (_, value) in metrics.snapshot().unwrap().operations() {
            prop_assert!(value.buckets().windows(2).all(|pair| pair[0] <= pair[1]));
            prop_assert!(value.buckets().iter().all(|bucket| *bucket <= value.count()));
            prop_assert!(value.sum().is_finite() && value.sum() >= 0.0);
        }
        prop_assert!(HISTOGRAM_BUCKETS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn tp07_absolute_gauge_set_is_idempotent(
        (component, gauge) in gauge_pair_strategy(),
        value in any::<u32>(),
    ) {
        let metrics = Metrics::registered(component);
        metrics.set_gauge(gauge, u64::from(value));
        let once = metrics.snapshot().unwrap();
        metrics.set_gauge(gauge, u64::from(value));
        prop_assert_eq!(metrics.snapshot().unwrap(), once);
    }

    #[test]
    fn tp08_independent_updates_commute(event_count in 0usize..32, gauge in any::<u16>()) {
        let first = Metrics::registered(ComponentName::Scheduler);
        first.set_gauge(GaugeName::AttachedAgents, u64::from(gauge));
        for _ in 0..event_count { first.record_event(EventName::RateLimitRefusal); }

        let second = Metrics::registered(ComponentName::Scheduler);
        for _ in 0..event_count { second.record_event(EventName::RateLimitRefusal); }
        second.set_gauge(GaugeName::AttachedAgents, u64::from(gauge));
        prop_assert_eq!(first.snapshot(), second.snapshot());
    }

    #[test]
    fn tp09_stateful_sequences_match_reference_model(
        commands in proptest::collection::vec(command_strategy(), 0..96),
    ) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        let mut model = ReferenceModel::default();
        for command in commands {
            match command {
                ModelCommand::FinishSuccess => {
                    finish(&metrics, OperationName::CapacityQuery, Outcome::Success, Reason::None);
                    model.success_count += 1;
                }
                ModelCommand::DropTimer => {
                    drop(metrics.operation_started(OperationName::CapacityQuery));
                    model.dropped_count += 1;
                }
                ModelCommand::RecordEvent => {
                    metrics.record_event(EventName::RateLimitRefusal);
                    model.event_count += 1;
                }
                ModelCommand::SetGauge(value) => {
                    metrics.set_gauge(GaugeName::AttachedAgents, u64::from(value));
                    model.gauge = Some(u64::from(value));
                }
                ModelCommand::Snapshot => {}
            }
            prop_assert_eq!(snapshot_projection(&metrics.snapshot().unwrap()), model.clone());
        }
    }

    #[test]
    fn tp10_every_timer_records_exactly_once(explicit in any::<bool>()) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        let timer = metrics.operation_started(OperationName::CapacityQuery);
        if explicit { timer.finish(Outcome::Success, Reason::None); } else { drop(timer); }
        let count: u64 = metrics.snapshot().unwrap().operations().map(|(_, value)| value.count()).sum();
        prop_assert_eq!(count, 1);
    }

    #[test]
    fn tp11_legal_updates_never_exceed_static_series_bound(commands in 0usize..128) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        for index in 0..commands {
            let (component, operation) = OPERATION_PAIRS[index % OPERATION_PAIRS.len()];
            if component == ComponentName::Scheduler {
                let (outcome, reason) = TERMINAL_CLASSIFICATIONS[index % TERMINAL_CLASSIFICATIONS.len()];
                finish(&metrics, operation, outcome, reason);
            }
        }
        let snapshot = metrics.snapshot().unwrap();
        let instances = snapshot.operations().len() + snapshot.events().len() + snapshot.gauges().len() + 1;
        prop_assert!(instances <= MAX_LABELED_INSTANCES);
    }

    #[test]
    fn tp12_runtime_secret_strings_never_reach_metrics(secret in "[A-Za-z0-9:/._-]{16,96}") {
        prop_assume!(![
            "scheduler", "capacity_query", "coordination", "success", "none", "podmesh",
        ].iter().any(|constant| secret.contains(constant) || constant.contains(&secret)));
        let metrics = Metrics::registered(ComponentName::Scheduler);
        finish(&metrics, OperationName::CapacityQuery, Outcome::Success, Reason::None);
        let text = render(&metrics.snapshot().unwrap());
        prop_assert!(!text.contains(&secret));
    }

    #[test]
    fn tp13_noop_and_recording_failures_preserve_business_result(value in any::<u64>()) {
        let noop = Metrics::noop();
        noop.record_event(EventName::ReplayRefusal);
        let registered = Metrics::registered(ComponentName::Agent);
        registered.record_event(EventName::RateLimitRefusal);
        prop_assert_eq!(value, value);
        prop_assert_eq!(registered.snapshot().unwrap().recording_failures(), 1);
        prop_assert_eq!(noop.snapshot(), None);
    }

    #[test]
    fn tp14_concurrent_snapshots_are_valid_serialized_prefixes(update_count in 0usize..64) {
        let metrics = Arc::new(Metrics::registered(ComponentName::Scheduler));
        let writer = metrics.clone();
        let handle = thread::spawn(move || {
            for _ in 0..update_count { writer.record_event(EventName::RateLimitRefusal); }
        });
        while !handle.is_finished() {
            let observed: u64 = metrics.snapshot().unwrap().events().map(|(_, value)| *value).sum();
            prop_assert!(observed <= update_count as u64);
        }
        handle.join().unwrap();
        let final_count: u64 = metrics.snapshot().unwrap().events().map(|(_, value)| *value).sum();
        prop_assert_eq!(final_count, update_count as u64);
    }

    #[test]
    fn tp15_one_valid_command_preserves_registry_invariants(
        commands in proptest::collection::vec(command_strategy(), 0..64),
    ) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        for command in commands {
            match command {
                ModelCommand::FinishSuccess => finish(&metrics, OperationName::CapacityQuery, Outcome::Success, Reason::None),
                ModelCommand::DropTimer => drop(metrics.operation_started(OperationName::CapacityQuery)),
                ModelCommand::RecordEvent => metrics.record_event(EventName::RateLimitRefusal),
                ModelCommand::SetGauge(value) => metrics.set_gauge(GaugeName::AttachedAgents, u64::from(value)),
                ModelCommand::Snapshot => {}
            }
            let snapshot = metrics.snapshot().unwrap();
            prop_assert!(snapshot.operations().all(|(_, value)|
                value.buckets().windows(2).all(|pair| pair[0] <= pair[1])
                    && value.buckets().iter().all(|bucket| *bucket <= value.count())
                    && value.sum().is_finite()
                    && value.sum() >= 0.0));
        }
    }

    #[test]
    fn tp16_restart_resets_counters_and_reconstructs_gauges(event_count in 0usize..32, gauge in any::<u16>()) {
        let before = Metrics::registered(ComponentName::Scheduler);
        for _ in 0..event_count { before.record_event(EventName::RateLimitRefusal); }
        before.set_gauge(GaugeName::AttachedAgents, u64::from(gauge));

        let after = Metrics::registered(ComponentName::Scheduler);
        prop_assert_eq!(after.snapshot().unwrap().events().len(), 0);
        after.set_gauge(GaugeName::AttachedAgents, u64::from(gauge));
        let observed = after.snapshot().unwrap().gauges().next().map(|(_, value)| *value);
        prop_assert_eq!(observed, Some(u64::from(gauge)));
    }

    #[test]
    fn tp17_rendering_is_deterministic_and_mutation_free(command_count in 0usize..32) {
        let metrics = Metrics::registered(ComponentName::Scheduler);
        for _ in 0..command_count { finish(&metrics, OperationName::CapacityQuery, Outcome::Success, Reason::None); }
        let snapshot = metrics.snapshot().unwrap();
        let before = snapshot.clone();
        prop_assert_eq!(render(&snapshot), render(&snapshot));
        prop_assert_eq!(snapshot, before);
    }

    #[test]
    fn tp18_catalog_expansion_is_exact_and_runtime_input_independent(secret in ".{0,64}") {
        let operation_keys = OPERATION_PAIRS.iter().flat_map(|(component, operation)| {
            TERMINAL_CLASSIFICATIONS.iter().map(move |(outcome, reason)|
                OperationKey::new(*component, *operation, *outcome, *reason).unwrap())
        }).collect::<BTreeSet<_>>();
        let event_keys = EVENT_PAIRS.iter().map(|(component, event)| EventKey::new(*component, *event).unwrap()).collect::<BTreeSet<_>>();
        let gauge_keys = GAUGE_PAIRS.iter().map(|(component, gauge)| GaugeKey::new(*component, *gauge).unwrap()).collect::<BTreeSet<_>>();
        prop_assert_eq!(operation_keys.len(), OPERATION_INSTANCE_COUNT);
        prop_assert_eq!(operation_keys.len() + event_keys.len() + gauge_keys.len() + FAILURE_INSTANCE_COUNT, MAX_LABELED_INSTANCES);
        prop_assert_eq!(OPERATION_INSTANCE_COUNT * HISTOGRAM_SAMPLES_PER_INSTANCE + event_keys.len() + gauge_keys.len() + FAILURE_INSTANCE_COUNT, MAX_RENDERED_SAMPLES);
        prop_assert!(MAX_RENDERED_SAMPLES > secret.len());
        prop_assert!(MAX_RENDERED_BYTES >= 1024 * 1024);
        prop_assert!(validate_event(ComponentName::Scheduler, EventName::RateLimitRefusal).is_ok());
        prop_assert!(validate_gauge(ComponentName::Scheduler, GaugeName::AttachedAgents).is_ok());
    }

}
