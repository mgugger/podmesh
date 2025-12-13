//! Integration tests for persistent state storage with redb.
//!
//! These tests verify that the storage module correctly:
//! - Persists manifest owner mappings across operations
//! - Persists deployment records
//! - Handles concurrent access safely
//! - Recovers state correctly

use podmesh_scheduler::storage::{StateStore, StateStoreConfig, DeploymentRecord};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

fn create_temp_db_path(test_name: &str) -> PathBuf {
    let temp_dir = std::env::temp_dir();
    temp_dir.join(format!(
        "podmesh_test_{}_{}_{}.redb",
        test_name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn create_test_store(test_name: &str) -> StateStore {
    let config = StateStoreConfig {
        db_path: create_temp_db_path(test_name),
    };
    StateStore::open(&config).expect("Failed to create test store")
}

#[test]
fn test_manifest_owner_persistence() {
    let store = create_test_store("manifest_owner_persistence");
    
    let manifest_id = "manifest-abc123";
    let owner_pubkey = vec![0x01, 0x02, 0x03, 0x04, 0x05];
    
    // Store owner
    store.set_manifest_owner(manifest_id, &owner_pubkey).unwrap();
    
    // Retrieve and verify
    let retrieved = store.get_manifest_owner(manifest_id).unwrap();
    assert_eq!(retrieved, Some(owner_pubkey.clone()));
    
    // Update owner
    let new_owner = vec![0x06, 0x07, 0x08];
    store.set_manifest_owner(manifest_id, &new_owner).unwrap();
    
    let updated = store.get_manifest_owner(manifest_id).unwrap();
    assert_eq!(updated, Some(new_owner));
    
    // Remove owner
    let removed = store.remove_manifest_owner(manifest_id).unwrap();
    assert!(removed.is_some());
    
    // Verify removal
    let after_remove = store.get_manifest_owner(manifest_id).unwrap();
    assert!(after_remove.is_none());
}

#[test]
fn test_deployment_record_lifecycle() {
    let store = create_test_store("deployment_lifecycle");
    
    let record = DeploymentRecord::new(
        "workload-xyz".to_string(),
        "manifest-123".to_string(),
        "Running".to_string(),
        "podman".to_string(),
        vec![0x11, 0x22, 0x33],
    );
    
    // Store deployment
    store.set_deployment(&record).unwrap();
    
    // Retrieve and verify
    let retrieved = store.get_deployment("workload-xyz").unwrap().unwrap();
    assert_eq!(retrieved.workload_id, "workload-xyz");
    assert_eq!(retrieved.manifest_id, "manifest-123");
    assert_eq!(retrieved.status, "Running");
    assert_eq!(retrieved.runtime_engine, "podman");
    
    // Update status
    store.update_deployment_status("workload-xyz", "Stopped").unwrap();
    let updated = store.get_deployment("workload-xyz").unwrap().unwrap();
    assert_eq!(updated.status, "Stopped");
    
    // Remove deployment
    let removed = store.remove_deployment("workload-xyz").unwrap();
    assert!(removed.is_some());
    
    // Verify removal
    let after_remove = store.get_deployment("workload-xyz").unwrap();
    assert!(after_remove.is_none());
}

#[test]
fn test_list_manifest_owners() {
    let store = create_test_store("list_owners");
    
    // Add multiple owners
    store.set_manifest_owner("m1", &[1, 2, 3]).unwrap();
    store.set_manifest_owner("m2", &[4, 5, 6]).unwrap();
    store.set_manifest_owner("m3", &[7, 8, 9]).unwrap();
    
    let owners = store.list_manifest_owners().unwrap();
    assert_eq!(owners.len(), 3);
    
    // Verify all manifest IDs are present
    let ids: Vec<&String> = owners.iter().map(|(id, _)| id).collect();
    assert!(ids.contains(&&"m1".to_string()));
    assert!(ids.contains(&&"m2".to_string()));
    assert!(ids.contains(&&"m3".to_string()));
}

#[test]
fn test_list_deployments() {
    let store = create_test_store("list_deployments");
    
    // Add multiple deployments
    for i in 1..=3 {
        let record = DeploymentRecord::new(
            format!("workload-{}", i),
            format!("manifest-{}", i),
            "Running".to_string(),
            "podman".to_string(),
            vec![i as u8],
        );
        store.set_deployment(&record).unwrap();
    }
    
    let deployments = store.list_deployments().unwrap();
    assert_eq!(deployments.len(), 3);
}

#[test]
fn test_get_deployments_by_manifest() {
    let store = create_test_store("deployments_by_manifest");
    
    // Add deployments for the same manifest (simulating replicas)
    for i in 1..=3 {
        let record = DeploymentRecord::new(
            format!("workload-replica-{}", i),
            "shared-manifest".to_string(),
            "Running".to_string(),
            "podman".to_string(),
            vec![0xAB],
        );
        store.set_deployment(&record).unwrap();
    }
    
    // Add a deployment for a different manifest
    let other = DeploymentRecord::new(
        "other-workload".to_string(),
        "other-manifest".to_string(),
        "Running".to_string(),
        "podman".to_string(),
        vec![0xCD],
    );
    store.set_deployment(&other).unwrap();
    
    // Query by manifest
    let by_manifest = store.get_deployments_by_manifest("shared-manifest").unwrap();
    assert_eq!(by_manifest.len(), 3);
    
    for deployment in by_manifest {
        assert_eq!(deployment.manifest_id, "shared-manifest");
    }
}

#[test]
fn test_concurrent_access() {
    let store = Arc::new(create_test_store("concurrent_access"));
    let mut handles = vec![];
    
    // Spawn multiple threads writing different manifest owners
    for i in 0..10 {
        let store_clone = Arc::clone(&store);
        let handle = thread::spawn(move || {
            let manifest_id = format!("concurrent-manifest-{}", i);
            let owner = vec![i as u8; 32];
            store_clone.set_manifest_owner(&manifest_id, &owner).unwrap();
            
            // Read back
            let retrieved = store_clone.get_manifest_owner(&manifest_id).unwrap();
            assert!(retrieved.is_some());
        });
        handles.push(handle);
    }
    
    // Wait for all threads
    for handle in handles {
        handle.join().expect("thread panicked");
    }
    
    // Verify all records are present
    let owners = store.list_manifest_owners().unwrap();
    assert_eq!(owners.len(), 10);
}

#[test]
fn test_ephemeral_store() {
    // Verify ephemeral stores work for testing
    let store1 = StateStore::open_ephemeral().expect("should create ephemeral store");
    let store2 = StateStore::open_ephemeral().expect("should create second ephemeral store");
    
    // Each store should be independent
    store1.set_manifest_owner("m1", &[1, 2, 3]).unwrap();
    
    let from_store2 = store2.get_manifest_owner("m1").unwrap();
    assert!(from_store2.is_none(), "ephemeral stores should be independent");
}

#[test]
fn test_nonexistent_records() {
    let store = create_test_store("nonexistent");
    
    // Getting nonexistent manifest owner should return None, not error
    let result = store.get_manifest_owner("does-not-exist").unwrap();
    assert!(result.is_none());
    
    // Getting nonexistent deployment should return None, not error
    let result = store.get_deployment("does-not-exist").unwrap();
    assert!(result.is_none());
    
    // Removing nonexistent records should succeed
    let result = store.remove_manifest_owner("does-not-exist").unwrap();
    assert!(result.is_none());
    
    let result = store.remove_deployment("does-not-exist").unwrap();
    assert!(result.is_none());
}
