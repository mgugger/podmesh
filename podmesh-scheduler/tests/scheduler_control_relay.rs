//! A scheduler that holds no attachment must still deliver control traffic.
//!
//! `podctl` picks whichever scheduler it can reach, but an agent attaches to
//! exactly one. Without a peer hop the client would be told the agent does not
//! exist by every scheduler except the one holding the attachment.

mod common;

use std::collections::HashSet;

use anyhow::{Result, ensure};
use common::{
    EchoAgentControl, TEST_TIMEOUT, TestScheduler, attach, endpoint, wait_for_attachment,
};
use iroh::{SecretKey, address_lookup::memory::MemoryLookup, protocol::Router};
use podmesh_scheduler::machine::ForwardError;
use protocol::{AGENT_CONTROL_ALPN, AgentControlOperation};
use tokio::time::timeout;

#[tokio::test]
async fn a_scheduler_without_the_attachment_relays_through_the_peer_that_has_it() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (entry, entry_identity, entry_keys) = common::scheduler_endpoint(&lookup).await?;
    let (holder, holder_identity, holder_keys) = common::scheduler_endpoint(&lookup).await?;
    let agent = endpoint(&SecretKey::generate(), &lookup).await?;
    for known in [&entry, &holder, &agent] {
        lookup.add_endpoint_info(known.addr());
    }
    let members = HashSet::from([entry.id(), holder.id()]);
    let entry_scheduler = TestScheduler::start(
        entry.clone(),
        entry_identity,
        entry_keys,
        members.clone(),
        Vec::new(),
    )
    .await?;
    // Locating an agent travels over gossip, so the two schedulers have to
    // share a gossip mesh before either can relay for the other.
    let holder_scheduler = TestScheduler::start(
        holder.clone(),
        holder_identity,
        holder_keys,
        members,
        vec![entry.id()],
    )
    .await?;

    let agent_router = Router::builder(agent.clone())
        .accept(AGENT_CONTROL_ALPN, EchoAgentControl)
        .spawn();
    let attachment = attach(&agent, &holder).await?;
    wait_for_attachment(&holder_scheduler.attachments, agent.id()).await?;
    ensure!(
        entry_scheduler
            .attachments
            .agent_addr(agent.id())
            .await
            .is_none(),
        "entry scheduler must not hold the agent attachment"
    );

    let payload = vec![11u8, 22, 33, 44];
    let mut expected = payload.clone();
    expected.reverse();
    let relayed = timeout(
        TEST_TIMEOUT,
        entry_scheduler.forwarder.forward(
            agent.id(),
            AgentControlOperation::Admission,
            payload.clone(),
        ),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("peer relay failed: {error}"))?;
    ensure!(
        relayed == expected,
        "relayed payload was not delivered intact"
    );
    // The answer to the gossiped query is kept, so a client running several
    // operations against one agent does not re-query the mesh every time.
    ensure!(
        entry_scheduler.locations.cached(agent.id()).await == Some(holder.id()),
        "the resolved location was not cached"
    );

    // A location that has gone stale must not keep failing forever: the next
    // request resolves again rather than reusing what is known to be wrong.
    entry_scheduler
        .locations
        .remember(agent.id(), entry.id())
        .await;
    let relayed = timeout(
        TEST_TIMEOUT,
        entry_scheduler.forwarder.forward(
            agent.id(),
            AgentControlOperation::Admission,
            payload.clone(),
        ),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("peer relay after a stale location failed: {error}"))?;
    ensure!(
        relayed == expected,
        "a stale cached location was not re-resolved"
    );

    // An agent nobody holds must fail closed rather than hang or succeed.
    let unknown = SecretKey::generate().public();
    let refused = timeout(
        TEST_TIMEOUT,
        entry_scheduler
            .forwarder
            .forward(unknown, AgentControlOperation::Command, payload),
    )
    .await?;
    ensure!(
        refused == Err(ForwardError::UnknownAgent),
        "an agent no scheduler holds must be reported as unknown"
    );

    attachment.close(0u8.into(), b"test complete");
    timeout(TEST_TIMEOUT, agent_router.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    entry_scheduler.shutdown().await?;
    holder_scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_relayed_request_from_a_non_member_scheduler_is_refused() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (holder, holder_identity, holder_keys) = common::scheduler_endpoint(&lookup).await?;
    let (stranger, stranger_identity, stranger_keys) = common::scheduler_endpoint(&lookup).await?;
    let agent = endpoint(&SecretKey::generate(), &lookup).await?;
    for known in [&holder, &stranger, &agent] {
        lookup.add_endpoint_info(known.addr());
    }
    // The stranger is deliberately absent from the holder's member allowlist.
    let holder_scheduler = TestScheduler::start(
        holder.clone(),
        holder_identity,
        holder_keys,
        HashSet::from([holder.id()]),
        Vec::new(),
    )
    .await?;
    let stranger_scheduler = TestScheduler::start(
        stranger.clone(),
        stranger_identity,
        stranger_keys,
        HashSet::from([stranger.id(), holder.id()]),
        vec![holder.id()],
    )
    .await?;

    let agent_router = Router::builder(agent.clone())
        .accept(AGENT_CONTROL_ALPN, EchoAgentControl)
        .spawn();
    let attachment = attach(&agent, &holder).await?;
    wait_for_attachment(&holder_scheduler.attachments, agent.id()).await?;

    let refused = timeout(
        TEST_TIMEOUT,
        stranger_scheduler.forwarder.forward(
            agent.id(),
            AgentControlOperation::Deploy,
            vec![7u8; 16],
        ),
    )
    .await?;
    ensure!(
        refused == Err(ForwardError::UnknownAgent),
        "a scheduler outside the member allowlist must not be able to relay"
    );

    attachment.close(0u8.into(), b"test complete");
    timeout(TEST_TIMEOUT, agent_router.shutdown()).await??;
    timeout(TEST_TIMEOUT, agent.close()).await?;
    stranger_scheduler.shutdown().await?;
    holder_scheduler.shutdown().await?;
    Ok(())
}
