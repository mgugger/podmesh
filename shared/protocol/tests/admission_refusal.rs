use protocol::agent::AdmissionRefusal;

#[test]
fn admission_refusal_vocabulary_preserves_wire_values_and_retry_policy() {
    let cases = [
        (
            AdmissionRefusal::StateRequiresRepair,
            "agent workload state requires repair",
            false,
        ),
        (
            AdmissionRefusal::AlreadyActive,
            "workload is already active or reserved",
            false,
        ),
        (
            AdmissionRefusal::WorkloadLimit,
            "agent workload limit reached",
            true,
        ),
        (
            AdmissionRefusal::ReservationLimit,
            "agent reservation limit reached",
            true,
        ),
        (
            AdmissionRefusal::InsufficientCapacity,
            "insufficient capacity",
            true,
        ),
        (
            AdmissionRefusal::PendingCapacityLimit,
            "pending reservation limit reached; retry once admissions settle",
            true,
        ),
    ];
    assert_eq!(cases.len(), AdmissionRefusal::ALL.len());
    for (refusal, wire, retryable) in cases {
        assert_eq!(refusal.as_str(), wire);
        assert_eq!(AdmissionRefusal::from_reason(wire), Some(refusal));
        assert_eq!(refusal.is_capacity(), retryable);
    }
    for unknown in [
        "",
        "capacity",
        "policy refused",
        "insufficient capacity ",
        "Insufficient capacity",
    ] {
        assert_eq!(AdmissionRefusal::from_reason(unknown), None);
    }
}
