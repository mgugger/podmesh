//! Persistent storage module for podmesh-scheduler using redb.
//!
//! This module provides ACID-compliant storage for:
//! - Manifest-owner mappings (for ownership verification)
//! - Deployment status (for recovery after scheduler restart)

mod state_store;

pub use state_store::{
    StateStore, StateStoreConfig, DeploymentRecord, StorageError,
    get_global_state_store, init_global_state_store,
};
