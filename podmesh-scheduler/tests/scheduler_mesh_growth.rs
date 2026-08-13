//! Membership has to grow past what any one operator wrote down.
//!
//! A large mesh cannot require every scheduler to be listed in every other
//! scheduler's configuration. Schedulers therefore announce themselves on the
//! gossip mesh, and a scheduler that hears an announcement admits the announcer
//! — which is safe because only an already-admitted member can be heard at all.

mod common;

use std::collections::HashSet;

use anyhow::{Result, ensure};
use common::{
    EchoAgentControl, TEST_TIMEOUT, TestScheduler, attach, endpoint, wait_for_attachment,
    wait_for_member,
};
use iroh::{SecretKey, address_lookup::memory::MemoryLookup, protocol::Router};
use protocol::{AGENT_CONTROL_ALPN, AgentControlOperation};
use tokio::time::timeout;

/// Two schedulers that were only ever configured to know a shared hub must end
/// up able to relay for each other, without either one being reconfigured.
#[tokio::test]
async fn schedulers_that_only_know_a_shared_hub_learn_each_other_from_gossip() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (hub, hub_identity, hub_keys) = common::scheduler_endpoint(&lookup).await?;
    let (left, left_identity, left_keys) = common::scheduler_endpoint(&lookup).await?;
    let (right, right_identity, right_keys) = common::scheduler_endpoint(&lookup).await?;
    let agent = endpoint(&SecretKey::generate(), &lookup).await?;
    for known in [&hub, &left, &right, &agent] {
        lookup.add_endpoint_info(known.addr());
    }

    let hub_scheduler = TestScheduler::start(
        hub.clone(),
        hub_identity,
        hub_keys,
        HashSet::from([hub.id(), left.id(), right.id()]),
        Vec::new(),
    )
    .await?;
    // Neither wing is told about the other: only the hub appears in their
    // member allowlists, which is what makes this a growth test.
    let left_scheduler = TestScheduler::start(
        left.clone(),
        left_identity,
        left_keys,
        HashSet::from([left.id(), hub.id()]),
        vec![hub.id()],
    )
    .await?;
    let right_scheduler = TestScheduler::start(
        right.clone(),
        right_identity,
        right_keys,
        HashSet::from([right.id(), hub.id()]),
        vec![hub.id()],
    )
    .await?;

    // Announcements are repeated in production; once is enough here because the
    // gossip mesh is already connected through the hub.
    left_scheduler.announce().await?;
    right_scheduler.announce().await?;
    wait_for_member(&left_scheduler.members(), right.id()).await?;
    wait_for_member(&right_scheduler.members(), left.id()).await?;

    // Membership alone proves little, so the learned peer is actually used: the
    // agent attaches to the right wing and the left wing relays through it.
    let agent_router = Router::builder(agent.clone())
        .accept(AGENT_CONTROL_ALPN, EchoAgentControl)
        .spawn();
    let attachment = attach(&agent, &right).await?;
    wait_for_attachment(&right_scheduler.attachments, agent.id()).await?;

    let payload = vec![5u8, 6, 7, 8];
    let mut expected = payload.clone();
    expected.reverse();
    let relayed = timeout(
        TEST_TIMEOUT,
        left_scheduler
            .forwarder
            .forward(agent.id(), AgentControlOperation::Admission, payload),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("relay through a gossip-learned peer failed: {error}"))?;
    ensure!(
        relayed == expected,
        "a scheduler learned from gossip did not relay the payload intact"
    );

    attachment.close(0u8.into(), b"test complete");
    timeout(TEST_TIMEOUT, agent_router.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    left_scheduler.shutdown().await?;
    right_scheduler.shutdown().await?;
    hub_scheduler.shutdown().await?;
    Ok(())
}
