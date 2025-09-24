use std::sync::Arc;

use axum::extract::Extension;
use axum::response::IntoResponse;
use axum::Json as AxumJson;

use openraft::raft::AppendEntriesRequest;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::VoteRequest;

use crate::gw_raft::app::App;
use crate::gw_raft::TypeConfig;

#[allow(clippy::unused_async)]
pub async fn vote(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<VoteRequest<TypeConfig>>,
) -> impl IntoResponse {
    let res = app.raft.vote(req).await;
    AxumJson(res)
}

#[allow(clippy::unused_async)]
pub async fn append(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<AppendEntriesRequest<TypeConfig>>,
) -> impl IntoResponse {
    let res = app.raft.append_entries(req).await;
    AxumJson(res)
}

#[allow(clippy::unused_async)]
pub async fn snapshot(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<InstallSnapshotRequest<TypeConfig>>,
) -> impl IntoResponse {
    let res = app.raft.install_snapshot(req).await;
    AxumJson(res)
}