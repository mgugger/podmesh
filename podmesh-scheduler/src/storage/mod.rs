//! Persistent storage module for podmesh-scheduler using redb.
//!
//! This module provides ACID-compliant storage for:
//! - Manifest-owner mappings (for ownership verification)
//! - Deployment status (for recovery after scheduler restart)
//! - Custodian records (DEK shares, per-manifest custody assignments)

mod state_store;
pub mod custodian_store;

pub use state_store::{
    StateStore, StateStoreConfig, DeploymentRecord, StorageError,
    get_global_state_store, init_global_state_store,
};
pub use custodian_store::{
    CustodianStore, CustodianRecord, CustodianStoreError,
    init_custodian_store, get_custodian_store,
};
