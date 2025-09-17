//! beemesh protocol crate — generated flatbuffers types live in `generated`.
mod generated {
    // Put each generated file into its own parent module to avoid duplicate nested `pub mod` names.
    pub mod generated_health {
        #![allow(
            dead_code,
            non_camel_case_types,
            non_snake_case,
            unused_imports,
            unused_variables,
            mismatched_lifetime_syntaxes
        )]
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/health_generated.rs"));
    }

    pub mod generated_capacity_request {
        #![allow(
            dead_code,
            non_camel_case_types,
            non_snake_case,
            unused_imports,
            unused_variables,
            mismatched_lifetime_syntaxes
        )]
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/capacity_request_generated.rs"));
    }

    pub mod generated_capacity_reply {
        #![allow(
            dead_code,
            non_camel_case_types,
            non_snake_case,
            unused_imports,
            unused_variables,
            mismatched_lifetime_syntaxes
        )]
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/capacity_reply_generated.rs"));
    }

    pub mod generated_apply_request {
        #![allow(
            dead_code,
            non_camel_case_types,
            non_snake_case,
            unused_imports,
            unused_variables,
            mismatched_lifetime_syntaxes
        )]
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/apply_request_generated.rs"));
    }

    pub mod generated_apply_response {
        #![allow(
            dead_code,
            non_camel_case_types,
            non_snake_case,
            unused_imports,
            unused_variables,
            mismatched_lifetime_syntaxes
        )]
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/apply_response_generated.rs"));
    }

    pub mod generated_handshake {
        #![allow(
            dead_code,
            non_camel_case_types,
            non_snake_case,
            unused_imports,
            unused_variables,
            mismatched_lifetime_syntaxes
        )]
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/generated/handshake_generated.rs"));
    }
}

pub mod flatbuffer {
    // Avoid glob imports; re-export specific items below.
    pub use crate::generated::generated_health::beemesh::{Health, root_as_health};
    pub use crate::generated::generated_capacity_reply::beemesh::{ CapacityReply, root_as_capacity_reply};
    pub use crate::generated::generated_capacity_request::beemesh::{ CapacityRequest, root_as_capacity_request};
    pub use crate::generated::generated_apply_request::beemesh::{ ApplyRequest, root_as_apply_request };
    pub use crate::generated::generated_apply_response::beemesh::{ ApplyResponse, root_as_apply_response };
    pub use crate::generated::generated_handshake::beemesh::{ Handshake, root_as_handshake };

    use flatbuffers::FlatBufferBuilder;

    // Macro to generate simple flatbuffer builder functions.
    // Usage:
    // fb_builder!(fn_name, fb_mod_path, FBType, FBArgsType, [field1:ty, field2:ty => string, ...]);
    // - fields marked with `=> string` will be converted with `fbb.create_string` and wrapped in `Some(...)`.
    macro_rules! fb_builder {
        // Variant with vector-of-strings support
        ($fn_name:ident, $args_path:path, $type_path:path,
         [ $( $pname:ident : $pty:ty ),* $(,)? ],
         [ $( $sname:ident : $sty:ty ),* $(,)? ],
         [ $( $vname:ident : $vty:ty ),* $(,)? ]
        ) => {
            pub fn $fn_name( $( $pname : $pty ),* , $( $sname : $sty ),* , $( $vname : $vty ),* ) -> Vec<u8> {
                let mut fbb = FlatBufferBuilder::with_capacity(256);
                $( let $sname = fbb.create_string($sname); )*
                // build vectors of string offsets for each vec field
                $(
                    let $vname = {
                        let mut tmp_vec: Vec<flatbuffers::WIPOffset<&str>> = Vec::with_capacity($vname.len());
                        for &s in $vname.iter() {
                            tmp_vec.push(fbb.create_string(s));
                        }
                        fbb.create_vector(&tmp_vec)
                    };
                )*
                let mut args: $args_path = Default::default();
                $( args.$pname = $pname; )*
                $( args.$sname = Some($sname); )*
                $( args.$vname = Some($vname); )*
                let off = <$type_path>::create(&mut fbb, &args);
                fbb.finish(off, None);
                fbb.finished_data().to_vec()
            }
        };

        // Backwards-compatible variant without vector fields
        ($fn_name:ident, $args_path:path, $type_path:path,
         [ $( $pname:ident : $pty:ty ),* $(,)? ],
         [ $( $sname:ident : $sty:ty ),* $(,)? ]
        ) => {
            pub fn $fn_name( $( $pname : $pty ),* , $( $sname : $sty ),* ) -> Vec<u8> {
                let mut fbb = FlatBufferBuilder::with_capacity(128);
                $( let $sname = fbb.create_string($sname); )*
                let mut args: $args_path = Default::default();
                $( args.$pname = $pname; )*
                $( args.$sname = Some($sname); )*
                let off = <$type_path>::create(&mut fbb, &args);
                fbb.finish(off, None);
                fbb.finished_data().to_vec()
            }
        };
    }

    fb_builder!(build_health,
        crate::generated::generated_health::beemesh::HealthArgs,
        crate::generated::generated_health::beemesh::Health,
        [ok: bool],
        [status: &str]
    );

    fb_builder!(build_capacity_request,
        crate::generated::generated_capacity_request::beemesh::CapacityRequestArgs,
        crate::generated::generated_capacity_request::beemesh::CapacityRequest,
        [cpu_milli: u32, memory_bytes: u64, storage_bytes: u64, replicas: u32],
        []
    );

    fb_builder!(build_capacity_reply,
        crate::generated::generated_capacity_reply::beemesh::CapacityReplyArgs,
        crate::generated::generated_capacity_reply::beemesh::CapacityReply,
        [ok: bool, cpu_available_milli: u32, memory_available_bytes: u64, storage_available_bytes: u64],
        [node_id: &str, region: &str],
        [capabilities: &[&str]]
    );

    fb_builder!(build_apply_request,
        crate::generated::generated_apply_request::beemesh::ApplyRequestArgs,
        crate::generated::generated_apply_request::beemesh::ApplyRequest,
        [replicas: u32],
        [tenant: &str, operation_id: &str, manifest_json: &str, origin_peer: &str]
    );

    fb_builder!(build_apply_response,
        crate::generated::generated_apply_response::beemesh::ApplyResponseArgs,
        crate::generated::generated_apply_response::beemesh::ApplyResponse,
        [ok: bool],
        [operation_id: &str, message: &str]
    );

    // Custom handshake builder since it only has string fields
    fb_builder!(build_handshake,
        crate::generated::generated_handshake::beemesh::HandshakeArgs,
        crate::generated::generated_handshake::beemesh::Handshake,
        [nonce: u32, timestamp: u64],
        [protocol_version: &str, signature: &str]
    );
}

pub mod libp2p_constants;

#[cfg(test)]
mod test {
    use crate::flatbuffer::build_health;

    use super::*;

    #[test]
    fn flatbuffers_health_roundtrip() {
        let buf = build_health(true, "healthy");
        // parse and verify
        let health = flatbuffer::root_as_health(&buf).unwrap();
        assert!(health.ok());
        assert_eq!(health.status().unwrap(), "healthy");
    }
}