//! Event-driven refill; one worker per daemon serializes refresh work.
use super::pool::{refresh, Slot, WarmPool};
use super::Options;
use crate::{Client, Error};
use rosen_protocol::{Endpoint, LogRequest};
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn run(pool: Arc<WarmPool>, endpoint: Endpoint) {
    // Only unused ready directories live here. Taken workspaces are moved
    // outside this root, so restart cleanup cannot remove an agent's files.
    loop {
        match reset_ready_root(&pool).await {
            Ok(()) => break,
            Err(error) => eprintln!("rosen warm pool startup: {error}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    loop {
        if let Err(error) = connected(&pool, &endpoint).await {
            eprintln!("rosen warm pool: {error}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connected(pool: &Arc<WarmPool>, endpoint: &Endpoint) -> Result<(), Error> {
    let client = Client::connect(endpoint).await?;
    // Subscribe before reading base: a concurrent land is either reflected
    // in the read or queued in the stream, never missed between the two.
    let mut events = client.subscribe(None).await?;
    loop {
        let retry_cleanup = cleanup(&client).await?;
        if let Err(error) = refill(pool, &client).await {
            if matches!(error, Error::Disconnected | Error::NotRunning(_)) {
                return Err(error);
            }
            eprintln!("rosen warm pool refill: {error}");
            // Retry transient store/index contention even if no later land
            // arrives to wake the worker. Successful reads are event-driven.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        tokio::select! {
            event = events.recv() => { event.ok_or(Error::Disconnected)?; }
            _ = pool.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)), if retry_cleanup => {}
        }
    }
}

async fn refill(pool: &Arc<WarmPool>, client: &Client) -> Result<(), Error> {
    // Read on every pass: a land can change the configured size.
    let configured = client.base_config().await?.warm_pool.size as usize;
    let mut state = pool.state.lock().await;
    state.size = state.requested.unwrap_or(configured);
    while state.ready.len() > state.size {
        let slot = state.ready.pop().expect("excess slot");
        std::fs::remove_dir_all(slot.path)?;
    }
    if state.size == 0 {
        return Ok(());
    }
    drop(state);
    let log = client
        .log(LogRequest {
            limit: Some(1),
            ..LogRequest::default()
        })
        .await?;
    let Some(base) = log.entries.first() else {
        return Ok(());
    };
    client.ensure_local(base.tree).await?;
    let store = client.local_store().await?;
    let mut state = pool.state.lock().await;
    refresh_ready(&store, &mut state.ready, base.tree).await?;
    // Bound one refill pass so a busy launch cannot starve landed events.
    let missing = state.size.saturating_sub(state.ready.len());
    for _ in 0..missing {
        if state.ready.len() >= state.size {
            break;
        }
        state.next += 1;
        let path = state
            .root
            .join(format!("{}-{}", std::process::id(), state.next));
        if path.exists() {
            continue;
        }
        // Existing ready slots remain available while a replacement copies.
        drop(state);
        let report = client
            .materialise_tree(base.tree, &path, Options::default())
            .await?;
        state = pool.state.lock().await;
        state.ready.push(Slot {
            path,
            tree: base.tree,
            files: report.files,
        });
    }
    Ok(())
}

async fn refresh_ready(
    store: &rosen_store::LooseObjectStore,
    ready: &mut Vec<Slot>,
    tree: rosen_core::ObjectId,
) -> Result<(), Error> {
    let mut retained = Vec::new();
    for mut slot in ready.drain(..) {
        let store = store.clone();
        let updated = tokio::task::spawn_blocking(move || {
            if slot.tree == tree || refresh(&store, &mut slot, tree).is_ok() {
                Some(slot)
            } else {
                // A failed partial refresh is never offered to an agent.
                let _ = std::fs::remove_dir_all(&slot.path);
                None
            }
        })
        .await
        .map_err(|error| Error::Unexpected(error.to_string()))?;
        if let Some(slot) = updated {
            retained.push(slot);
        }
    }
    *ready = retained;
    Ok(())
}

async fn reset_ready_root(pool: &WarmPool) -> Result<(), Error> {
    let root = std::path::absolute(&pool.state.lock().await.root)?;
    tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    })
    .await
    .map_err(|error| Error::Unexpected(error.to_string()))??;
    Ok(())
}

async fn cleanup(client: &Client) -> Result<bool, Error> {
    let mut pending = false;
    for report in client.cleanup_workspaces().await? {
        if let Some(error) = report.error {
            pending = true;
            eprintln!("rosen workspace cleanup {}: {error}", report.path.display());
        }
    }
    Ok(pending)
}
