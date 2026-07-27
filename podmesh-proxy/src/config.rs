#[derive(Clone, Debug)]
pub struct Config {
    pub proxy_peer_multiaddrs: Vec<String>,
    pub identity: p2p::identity::IdentitySource,
    pub libp2p_quic_port: u16,
    pub libp2p_host: String,
    pub rest_host: String,
    pub rest_port: u16,
    pub disable_rest_api: bool,
    pub enable_ingress: bool,
    /// Base64-encoded Ed25519 owner public key used for static tenant authorization.
    pub owner_pubkey: Option<String>,
}

impl Config {
    pub fn apply_defaults(&mut self) {
        if self.libp2p_host.is_empty() {
            self.libp2p_host = "0.0.0.0".to_string();
        }
        if self.rest_host.is_empty() {
            self.rest_host = "0.0.0.0".to_string();
        }
        if self.rest_port == 0 {
            self.rest_port = 7100;
        }
    }
}
