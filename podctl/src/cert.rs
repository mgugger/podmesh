use anyhow::Context;
use clap::Subcommand;
use protocol::{NodeCert, NodeRole};
use std::str::FromStr;

#[derive(Subcommand, Debug)]
pub enum CertCommands {
    /// Issue a new NodeCert signed with the owner's key
    Issue {
        #[arg(long)]
        peer_id: String,
        /// Path to KEM public key file (raw bytes)
        #[arg(long)]
        kem_pub: String,
        /// Path to signing public key file (raw bytes)
        #[arg(long)]
        sign_pub: String,
        /// Path to signing private key file (raw bytes)
        #[arg(long)]
        sign_sk: String,
        #[arg(long, value_delimiter = ',')]
        caps: Vec<String>,
        #[arg(long, default_value = "both")]
        role: String,
        #[arg(long, default_value = "365")]
        valid_days: u64,
        /// Output path (default: node_cert.bin)
        #[arg(long)]
        output: Option<String>,
    },
    /// Show the contents of a NodeCert
    Show {
        path: String,
    },
    /// Verify a NodeCert's owner signature
    Verify {
        cert_path: String,
        /// Path to owner public key file (raw bytes)
        #[arg(long)]
        owner_pub: String,
    },
}

pub fn handle_cert_command(cmd: CertCommands) -> anyhow::Result<()> {
    match cmd {
        CertCommands::Issue {
            peer_id,
            kem_pub,
            sign_pub,
            sign_sk,
            caps,
            role,
            valid_days,
            output,
        } => {
            let kem_pub_bytes = std::fs::read(&kem_pub)
                .with_context(|| format!("reading kem_pub from {}", kem_pub))?;
            let sign_pub_bytes = std::fs::read(&sign_pub)
                .with_context(|| format!("reading sign_pub from {}", sign_pub))?;
            let sign_sk_bytes = std::fs::read(&sign_sk)
                .with_context(|| format!("reading sign_sk from {}", sign_sk))?;

            let node_role = NodeRole::from_str(&role)?;
            let valid_until = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + valid_days * 86400;

            let cert = NodeCert {
                peer_id,
                kem_pubkey: crypto::b64_encode(&kem_pub_bytes),
                signing_pubkey: crypto::b64_encode(&sign_pub_bytes),
                capabilities: caps,
                role: node_role,
                valid_until,
                owner_pubkey: crypto::b64_encode(&sign_pub_bytes),
                owner_sig: String::new(),
                endorsements: vec![],
            };

            let signed = cert.sign(&sign_sk_bytes, &sign_pub_bytes)?;
            let out_path = output.unwrap_or_else(|| "node_cert.bin".to_string());
            protocol::node_cert::save_node_cert(
                std::path::Path::new(&out_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("."),
                &signed,
            )?;
            // save_node_cert writes to key_dir/node_cert.bin; if custom output path differs, rename
            let default_path = protocol::node_cert::default_node_cert_path(
                std::path::Path::new(&out_path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or("."),
            );
            let target_path = std::path::PathBuf::from(&out_path);
            if default_path != target_path {
                std::fs::rename(&default_path, &target_path)?;
            }

            println!("NodeCert issued and saved to: {}", out_path);
            println!("  peer_id:    {}", signed.peer_id);
            println!("  role:       {}", signed.role);
            println!("  valid_until: {}", signed.valid_until);
            println!("  caps:       {:?}", signed.capabilities);
        }
        CertCommands::Show { path } => {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading cert from {}", path))?;
            let cert = NodeCert::from_bytes(&bytes)
                .with_context(|| "deserializing NodeCert")?;
            println!("NodeCert:");
            println!("  peer_id:       {}", cert.peer_id);
            println!("  role:          {}", cert.role);
            println!("  valid_until:   {}", cert.valid_until);
            println!("  expired:       {}", cert.is_expired());
            println!("  capabilities:  {:?}", cert.capabilities);
            println!("  kem_pubkey:    {}", cert.kem_pubkey);
            println!("  signing_pubkey:{}", cert.signing_pubkey);
            println!("  owner_pubkey:  {}", cert.owner_pubkey);
            println!("  owner_sig:     {}", cert.owner_sig);
            println!("  endorsements:  {}", cert.endorsements.len());
        }
        CertCommands::Verify { cert_path, owner_pub } => {
            let bytes = std::fs::read(&cert_path)
                .with_context(|| format!("reading cert from {}", cert_path))?;
            let cert = NodeCert::from_bytes(&bytes)
                .with_context(|| "deserializing NodeCert")?;

            // Override owner_pubkey from file for verification
            let owner_pk_bytes = std::fs::read(&owner_pub)
                .with_context(|| format!("reading owner_pub from {}", owner_pub))?;
            let mut check_cert = cert.clone();
            check_cert.owner_pubkey = crypto::b64_encode(&owner_pk_bytes);
            check_cert.verify()?;
            println!("NodeCert signature is valid.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod cert_tests {
    use super::*;
    use crypto::ensure_keypair_ephemeral;

    fn make_signed_cert(role: NodeRole) -> (NodeCert, Vec<u8>, Vec<u8>) {
        let (pk, sk) = ensure_keypair_ephemeral().unwrap();
        let (kem_pk, _) = ensure_keypair_ephemeral().unwrap();
        let cert = NodeCert {
            peer_id: "QmTest".to_string(),
            kem_pubkey: crypto::b64_encode(&kem_pk),
            signing_pubkey: crypto::b64_encode(&pk),
            capabilities: vec!["test".to_string()],
            role,
            valid_until: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 86400,
            owner_pubkey: crypto::b64_encode(&pk),
            owner_sig: String::new(),
            endorsements: vec![],
        };
        let signed = cert.sign(&sk, &pk).unwrap();
        (signed, sk, pk)
    }

    #[test]
    fn test_cert_issue_and_verify() {
        let (cert, _sk, _pk) = make_signed_cert(NodeRole::Both);
        assert!(cert.verify().is_ok());
    }

    #[test]
    fn test_cert_rejects_wrong_owner_key() {
        let (cert, _sk, _pk) = make_signed_cert(NodeRole::Worker);
        let (wrong_pk, _) = ensure_keypair_ephemeral().unwrap();
        let mut tampered = cert.clone();
        tampered.owner_pubkey = crypto::b64_encode(&wrong_pk);
        assert!(tampered.verify().is_err());
    }
}
