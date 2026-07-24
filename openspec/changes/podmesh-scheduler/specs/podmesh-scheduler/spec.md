# podmesh-scheduler

## ADDED Requirements

### Requirement: Scheduler is stateless

The scheduler MUST NOT persist workload or agent state and MUST tolerate restart without affecting
running workloads.

#### Scenario: Scheduler restarts
- **WHEN** a scheduler process restarts with an empty registry
- **THEN** running workloads remain unaffected
- **AND** agents republish their advertisements

### Requirement: Scheduler validates agent advertisements

The scheduler MUST verify advertisement signature, expiry, field bounds, and monotonic expiry before
registration.

#### Scenario: Agent registers
- **WHEN** the scheduler receives an advertisement
- **THEN** it verifies signature, bounds, expiry, and monotonic freshness before storing it in memory

### Requirement: Scheduler does not receive workload information

Candidate selection MUST operate only on public agent advertisements. Workload admission and
deployment MUST occur directly between the namespace client and selected agent.

#### Scenario: Client requests placement
- **WHEN** podctl asks for a candidate
- **THEN** the scheduler evaluates only public agent advertisements
- **AND** receives no workload requirements, ciphertext, keys, status, or logs

### Requirement: Initial placement selects one agent

The scheduler MUST select one available, non-excluded agent deterministically by coarse load and
node ID. Replica placement is not required in this version.

#### Scenario: Multiple agents are available
- **WHEN** several non-excluded advertisements are valid
- **THEN** the scheduler selects one by lowest coarse load and deterministic node ID ordering