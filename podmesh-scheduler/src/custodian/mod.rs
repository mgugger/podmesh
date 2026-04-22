//! Custodian subsystem for podmesh-scheduler.
//!
//! This module is active only when the node is started with `--mode custodian` or `--mode both`.
//!
//! Submodules:
//! - `coordinator` — rendezvous-hash coordinator election
//! - `heartbeat`   — HeartbeatPing builder + CustodianLivenessTracker
//! - `oracle_v2`   — ShamirOracle: the concrete KeyReleaseOracle impl (Shamir secret sharing)
//! - `sealer`      — unseal_spec: share collection + spec decryption

pub mod coordinator;
pub mod heartbeat;
pub mod oracle_v2;
pub mod sealer;
