use std::sync::Arc;

use axum::extract::Extension;
use axum::response::IntoResponse;
use axum::Json as AxumJson;
use axum::http::StatusCode;
use serde_json::json;

use openraft::error::decompose::DecomposeResult;
use openraft::error::CheckIsLeaderError;
use openraft::error::Infallible;
use openraft::ReadPolicy;

use crate::gw_raft::app::App;
use crate::gw_raft::store::Request;
use crate::gw_raft::TypeConfig;

#[allow(clippy::unused_async)]
pub async fn write(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<Request>,
) -> impl IntoResponse {
    let response = app.raft.client_write(req).await.decompose().unwrap();
    AxumJson(response)
}

pub async fn read(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<String>,
) -> impl IntoResponse {
    let state_machine = app.state_machine_store.state_machine.read().await;
    let key = req;
    let value = state_machine.data.get(&key).cloned();

    let res: Result<String, Infallible> = Ok(value.unwrap_or_default());
    AxumJson(res)
}

pub async fn linearizable_read(
    Extension(app): Extension<Arc<App>>,
    AxumJson(req): AxumJson<String>,
) -> impl IntoResponse {
    let ret = app.raft.get_read_linearizer(ReadPolicy::ReadIndex).await.decompose().unwrap();

    match ret {
        Ok(linearizer) => {
            linearizer.await_ready(&app.raft).await.unwrap();

            let state_machine = app.state_machine_store.state_machine.read().await;
            let key = req;
            let value = state_machine.data.get(&key).cloned();

            let res: Result<String, CheckIsLeaderError<TypeConfig>> = Ok(value.unwrap_or_default());
            AxumJson(res)
        }
        Err(e) => AxumJson(Err(e)),
    }
}

pub async fn health_check(
    Extension(app): Extension<Arc<App>>,
) -> impl IntoResponse {
    let ret = app.raft.get_read_linearizer(ReadPolicy::ReadIndex).await.decompose().unwrap();

    match ret {
        Ok(linearizer) => {
            let ready = linearizer.await_ready(&app.raft).await;

            if ready.is_ok() {
                (
                    StatusCode::OK,
                    AxumJson(json!({"ok": true, "status": "healthy"})),
                )
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    AxumJson(json!({"ok": false, "status": "unhealthy"})),
                )
            }
        }
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            AxumJson(json!({"ok": false, "error": format!("{:?}", e)})),
        ),
    }
}