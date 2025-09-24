use std::sync::Arc;

use axum::extract::Extension;
use axum::response::IntoResponse;
use axum::Json as AxumJson;

use openraft::error::decompose::DecomposeResult;
use openraft::error::Infallible;
use openraft::BasicNode;
use openraft::RaftMetrics;

use crate::gw_raft::app::App;
use crate::gw_raft::NodeId;
use crate::gw_raft::TypeConfig;

// --- Cluster management

/// Add a node as **Learner**.
///
/// A Learner receives log replication from the leader but does not vote.
/// This should be done before adding a node as a member into the cluster
/// (by calling `change-membership`)
pub async fn add_learner(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<(NodeId, String)>,
) -> impl IntoResponse {
    let node_id = req .0;
    let node = BasicNode { addr: req .1.clone() };
    let res = app.raft.add_learner(node_id, node, true).await.decompose().unwrap();
    AxumJson(res)
}

/// Changes specified learners to members, or remove members.
pub async fn change_membership(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<Vec<NodeId>>,
) -> impl IntoResponse {
    let set: std::collections::BTreeSet<NodeId> = req.into_iter().collect();
    let res = app.raft.change_membership(set, false).await.decompose().unwrap();
    AxumJson(res)
}

/// Initialize a single-node cluster if the `req` is empty vec.
/// Otherwise initialize a cluster with the `req` specified vec of node-id and node-address
pub async fn init(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<Vec<(NodeId, String)>>,
) -> impl IntoResponse {
    let mut nodes = std::collections::BTreeMap::new();
    if req.is_empty() {
        nodes.insert(app.id, BasicNode { addr: app.addr.clone() });
    } else {
        for (id, addr) in req.into_iter() {
            nodes.insert(id, BasicNode { addr });
        }
    };
    let res = app.raft.initialize(nodes).await.decompose().unwrap();
    AxumJson(res)
}

/// Get the latest metrics of the cluster
pub async fn metrics(
    Extension(app): Extension<Arc<App>>,
) -> impl IntoResponse {
    let metrics = app.raft.metrics().borrow().clone();
    let res: Result<RaftMetrics<TypeConfig>, Infallible> = Ok(metrics);
    AxumJson(res)
}