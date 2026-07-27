use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum SidecarRouteKind {
    #[default]
    Service = 0,
    Ingress = 1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarRouteSpec {
    pub host: String,
    pub path_prefix: String,
    pub target_port: u16,
    pub service_name: String,
    pub service_port: String,
    pub source: SidecarRouteKind,
}
