mod common;

use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use common::{TEST_TIMEOUT, config, endpoint, now_secs, signed_query};
use iroh::{SecretKey, address_lookup::memory::MemoryLookup};
use podmesh_scheduler::machine::{
    AttachmentManager, PlacementHandler, QueryManager, SCHEDULER_GOSSIP_ALPN, SchedulerGossip,
};
use tokio::time::timeout;

#[tokio::test]
async fn authorized_schedulers_exchange_queries_and_unauthorized_peer_is_closed() -> Result<()> {
    let lookup = MemoryLookup::new();
    let first_secret = SecretKey::generate();
    let second_secret = SecretKey::generate();
    let unauthorized_secret = SecretKey::generate();
    let first = endpoint(&first_secret, &lookup).await?;
    let second = endpoint(&second_secret, &lookup).await?;
    let unauthorized = endpoint(&unauthorized_secret, &lookup).await?;
    for endpoint in [&first, &second, &unauthorized] {
        lookup.add_endpoint_info(endpoint.addr());
    }

    let members = HashSet::from([first.id(), second.id()]);
    let first_config = config(members.clone(), Vec::new());
    let second_config = config(members, vec![first.id()]);
    let first_queries = QueryManager::new(16, 8, TEST_TIMEOUT);
    let second_queries = QueryManager::new(16, 8, TEST_TIMEOUT);
    let first_gossip = SchedulerGossip::start(
        first.clone(),
        &first_config,
        AttachmentManager::new(16, 8, TEST_TIMEOUT).handler(),
        first_queries.offer_handler(),
        PlacementHandler::new(16, TEST_TIMEOUT),
    )
    .await?;
    let second_gossip = SchedulerGossip::start(
        second.clone(),
        &second_config,
        AttachmentManager::new(16, 8, TEST_TIMEOUT).handler(),
        second_queries.offer_handler(),
        PlacementHandler::new(16, TEST_TIMEOUT),
    )
    .await?;
    let mut second_events = second_gossip.subscribe_queries();

    let query = signed_query(first.id().as_bytes(), now_secs())?;
    first_gossip.publish(query.clone()).await?;
    ensure!(
        timeout(TEST_TIMEOUT, second_events.recv())
            .await
            .context("authorized scheduler did not receive query")??
            == query,
        "authorized scheduler received another query"
    );

    let connection = timeout(
        TEST_TIMEOUT,
        unauthorized.connect(first.addr(), SCHEDULER_GOSSIP_ALPN),
    )
    .await
    .context("unauthorized connection attempt timed out")??;
    timeout(TEST_TIMEOUT, connection.closed())
        .await
        .context("unauthorized scheduler connection was not closed")?;

    timeout(TEST_TIMEOUT, first_gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, second_gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, first.close()).await?;
    timeout(TEST_TIMEOUT, second.close()).await?;
    timeout(TEST_TIMEOUT, unauthorized.close()).await?;
    Ok(())
}

#[tokio::test]
async fn scheduler_rejoins_after_partition_with_the_same_identity() -> Result<()> {
    let lookup = MemoryLookup::new();
    let first_secret = SecretKey::generate();
    let second_secret = SecretKey::generate();
    let first = endpoint(&first_secret, &lookup).await?;
    let second = endpoint(&second_secret, &lookup).await?;
    lookup.add_endpoint_info(first.addr());
    lookup.add_endpoint_info(second.addr());
    let members = HashSet::from([first.id(), second.id()]);
    let first_config = config(members.clone(), Vec::new());
    let second_config = config(members, vec![first.id()]);
    let first_queries = QueryManager::new(8, 8, TEST_TIMEOUT);
    let first_gossip = SchedulerGossip::start(
        first.clone(),
        &first_config,
        AttachmentManager::new(8, 8, TEST_TIMEOUT).handler(),
        first_queries.offer_handler(),
        PlacementHandler::new(8, TEST_TIMEOUT),
    )
    .await?;
    let second_queries = QueryManager::new(8, 8, TEST_TIMEOUT);
    let second_gossip = SchedulerGossip::start(
        second.clone(),
        &second_config,
        AttachmentManager::new(8, 8, TEST_TIMEOUT).handler(),
        second_queries.offer_handler(),
        PlacementHandler::new(8, TEST_TIMEOUT),
    )
    .await?;
    timeout(TEST_TIMEOUT, second_gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, second.close()).await?;

    let recovered = endpoint(&second_secret, &lookup).await?;
    ensure!(
        recovered.id() == second_secret.public(),
        "scheduler identity changed on recovery"
    );
    lookup.add_endpoint_info(recovered.addr());
    let recovered_queries = QueryManager::new(8, 8, TEST_TIMEOUT);
    let recovered_gossip = SchedulerGossip::start(
        recovered.clone(),
        &second_config,
        AttachmentManager::new(8, 8, TEST_TIMEOUT).handler(),
        recovered_queries.offer_handler(),
        PlacementHandler::new(8, TEST_TIMEOUT),
    )
    .await?;
    let mut recovered_events = recovered_gossip.subscribe_queries();
    let query = signed_query(first.id().as_bytes(), now_secs())?;
    first_gossip.publish(query.clone()).await?;
    ensure!(
        timeout(TEST_TIMEOUT, recovered_events.recv()).await?? == query,
        "recovered scheduler received another query"
    );

    timeout(TEST_TIMEOUT, recovered_gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, first_gossip.shutdown()).await??;
    timeout(TEST_TIMEOUT, recovered.close()).await?;
    timeout(TEST_TIMEOUT, first.close()).await?;
    Ok(())
}
