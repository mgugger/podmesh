use std::time::Duration;

use protocol::{
    IrohFrame, MAX_IROH_FRAME_BYTES, OperationFrame, OperationKind, TenantSessionFrame,
    WorkloadPeerRole, read_iroh_frame, write_iroh_frame,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(1);

fn session_frame() -> IrohFrame {
    IrohFrame::TenantSession(TenantSessionFrame {
        tenant_owner: "tenant-owner".into(),
        role: WorkloadPeerRole::Sidecar,
        subject_endpoint_id: vec![1; 32],
        audience_endpoint_id: vec![2; 32],
        proxy_grant: Vec::new(),
        mesh_join_biscuit: vec![3; 128],
    })
}

#[tokio::test]
async fn bounded_frame_roundtrips() {
    let (mut writer, mut reader) = tokio::io::duplex(4096);
    let cancellation = CancellationToken::new();
    let expected = session_frame();
    let (write_result, read_result) = tokio::join!(
        write_iroh_frame(&mut writer, &expected, TEST_TIMEOUT, &cancellation),
        read_iroh_frame(&mut reader, TEST_TIMEOUT, &cancellation),
    );
    write_result.unwrap();
    assert_eq!(read_result.unwrap(), expected);
}

#[tokio::test]
async fn oversized_kind_is_rejected_before_write() {
    let (mut writer, _reader) = tokio::io::duplex(64);
    let frame = IrohFrame::Operation(OperationFrame {
        kind: OperationKind::Egress,
        workload_id: "workload".into(),
        biscuit: vec![1],
        payload: vec![0; protocol::iroh_frame::MAX_EGRESS_FRAME_BYTES],
    });
    assert!(
        write_iroh_frame(&mut writer, &frame, TEST_TIMEOUT, &CancellationToken::new(),)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn oversized_declared_length_is_rejected_before_allocation() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    writer
        .write_all(
            &u32::try_from(MAX_IROH_FRAME_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        )
        .await
        .unwrap();
    assert!(
        read_iroh_frame(&mut reader, TEST_TIMEOUT, &CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn truncated_payload_is_rejected() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    writer.write_all(&10u32.to_be_bytes()).await.unwrap();
    writer.write_all(&[1, 2]).await.unwrap();
    drop(writer);
    assert!(
        read_iroh_frame(&mut reader, TEST_TIMEOUT, &CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn blocked_read_respects_deadline() {
    let (_writer, mut reader) = tokio::io::duplex(64);
    assert!(
        read_iroh_frame(
            &mut reader,
            Duration::from_millis(10),
            &CancellationToken::new(),
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn cancelled_read_stops_immediately() {
    let (_writer, mut reader) = tokio::io::duplex(64);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        read_iroh_frame(&mut reader, TEST_TIMEOUT, &cancellation)
            .await
            .is_err()
    );
}
