use actix_web::post;
use actix_web::get;
use actix_web::web;
use actix_web::web::Data;
use actix_web::Responder;
use actix_web::HttpResponse;
use openraft::error::decompose::DecomposeResult;
use openraft::error::CheckIsLeaderError;
use openraft::error::Infallible;
use openraft::ReadPolicy;
use web::Json;

use crate::gw_raft::app::App;
use crate::gw_raft::store::Request;
use crate::gw_raft::TypeConfig;

/**
 * Application API
 *
 * This is where you place your application, you can use the example below to create your
 * API. The current implementation:
 *
 *  - `POST - /write` saves a value in a key and sync the nodes.
 *  - `POST - /read` attempt to find a value from a given key.
 */
#[post("/write")]
pub async fn write(app: Data<App>, req: Json<Request>) -> actix_web::Result<impl Responder> {
    let response = app.raft.client_write(req.0).await.decompose().unwrap();
    Ok(Json(response))
}

#[post("/read")]
pub async fn read(app: Data<App>, req: Json<String>) -> actix_web::Result<impl Responder> {
    let state_machine = app.state_machine_store.state_machine.read().await;
    let key = req.0;
    let value = state_machine.data.get(&key).cloned();

    let res: Result<String, Infallible> = Ok(value.unwrap_or_default());
    Ok(Json(res))
}

#[post("/linearizable_read")]
pub async fn linearizable_read(app: Data<App>, req: Json<String>) -> actix_web::Result<impl Responder> {
    let ret = app.raft.get_read_linearizer(ReadPolicy::ReadIndex).await.decompose().unwrap();

    match ret {
        Ok(linearizer) => {
            linearizer.await_ready(&app.raft).await.unwrap();

            let state_machine = app.state_machine_store.state_machine.read().await;
            let key = req.0;
            let value = state_machine.data.get(&key).cloned();

            let res: Result<String, CheckIsLeaderError<TypeConfig>> = Ok(value.unwrap_or_default());
            Ok(Json(res))
        }
        Err(e) => Ok(Json(Err(e))),
    }
}

#[get("/health")]
pub async fn health_check(app: Data<App>) -> actix_web::Result<impl Responder> {
    // Use the read linearizer (ReadIndex) to check for a healthy leader that can serve
    // linearizable reads. If there is no leader or the leader is not available this will
    // return a `CheckIsLeaderError` which we'll propagate as an unhealthy result.
    let ret = app.raft.get_read_linearizer(ReadPolicy::ReadIndex).await.decompose().unwrap();

    match ret {
        Ok(linearizer) => {
            // await_ready returns Result<(), CheckIsLeaderError<TypeConfig>>.
            let ready = linearizer.await_ready(&app.raft).await;

            if ready.is_ok() {
                // healthy: return 200 and a consistent JSON shape
                Ok(HttpResponse::Ok().json(serde_json::json!({"ok": true, "status": "healthy"})))
            } else {
                // unhealthy: return 503 Service Unavailable and JSON
                Ok(HttpResponse::ServiceUnavailable().json(serde_json::json!({"ok": false, "status": "unhealthy"})))
            }
        }
        Err(e) => {
            // error while obtaining linearizer: treat as unhealthy
            Ok(HttpResponse::ServiceUnavailable().json(serde_json::json!({"ok": false, "error": format!("{:?}", e)})))
        }
    }
}