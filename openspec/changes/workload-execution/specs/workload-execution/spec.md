# Workload Execution

## ADDED Requirements

### Requirement: Encrypted workload execution

The system MUST allow execution of encrypted workloads without exposing plaintext to schedulers.

#### Scenario: Worker executes encrypted workload
- Given a workload is submitted in encrypted form
- And a worker is assigned the workload
- When the worker retrieves it
- Then only the worker can decrypt and execute it

## ADDED Requirements

### Requirement: Scheduler cannot access workload secrets

Schedulers MUST NOT access plaintext workloads or decryption keys.

#### Scenario: Scheduler routes workload without decryption
- Given a workload is submitted
- When the scheduler processes it
- Then it forwards it without decrypting or inspecting payload
