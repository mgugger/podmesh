//! beemesh protocol crate — generated flatbuffers types live in `generated`.
pub mod generated {
    #![allow(dead_code, non_camel_case_types, non_snake_case, unused_imports, unused_variables, mismatched_lifetime_syntaxes)]
    // include the generated Rust file(s) under src/generated
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/health_generated.rs"));
}

pub mod libp2p_constants;

use flatbuffers::FlatBufferBuilder;

/// Build a real FlatBuffer `Status` message as bytes using the generated builder.
pub fn build_health(ok: bool, status: &str) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(64);
    let status_str = fbb.create_string(status);
    let args = generated::beemesh::HealthArgs { ok, status: Some(status_str) };
    let off = generated::beemesh::Health::create(&mut fbb, &args);
    generated::beemesh::finish_health_buffer(&mut fbb, off);
    fbb.finished_data().to_vec()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn flatbuffers_health_roundtrip() {
        let buf = build_health(true, "healthy");
        // parse and verify
        let health = generated::beemesh::root_as_health(&buf).unwrap();
        assert!(health.ok());
        assert_eq!(health.status().unwrap(), "healthy");
    }
}
