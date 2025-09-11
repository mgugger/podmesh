use axum::{
    extract::{State, Path},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::watch};
use crate::pod_communication;

pub fn build_router() -> Router {
    Router::new()
}