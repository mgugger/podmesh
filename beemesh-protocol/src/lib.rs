//! beemesh protocol crate — generated flatbuffers types live in `generated`.

pub mod generated {
    // include the generated Rust file(s) under src/generated
    include!("generated/status_generated.rs");
}

use flatbuffers::FlatBufferBuilder;

/// Build a real FlatBuffer `Status` message as bytes using the generated builder.
pub fn build_status(ok: bool, status: &str) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new_with_capacity(64);
    let status_str = fbb.create_string(status);
    let args = generated::beemesh::StatusArgs { ok, status: Some(status_str) };
    let off = generated::beemesh::Status::create(&mut fbb, &args);
    generated::beemesh::finish_status_buffer(&mut fbb, off);
    fbb.finished_data().to_vec()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn flatbuffers_status_roundtrip() {
        let buf = build_status(true, "healthy");
        // parse and verify
        let status = generated::beemesh::root_as_status(&buf).unwrap();
        assert!(status.ok());
        assert_eq!(status.status().unwrap(), "healthy");
    }
}
