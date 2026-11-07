//! Relay access, a thin layer over nostr-sdk.

use std::time::Duration;

use anyhow::bail;
use futures_util::StreamExt;
use nostr::prelude::{Event, Filter, Kind, PublicKey, RelayUrl};
use nostr_sdk::prelude::{Client, ClientNotification};
use tokio::sync::mpsc;

use crate::Result;

pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "wss://relay.ditto.pub",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.damus.io",
];

pub struct Relays {
    client: Client,
    pub urls: Vec<String>,
}

impl Relays {
    pub async fn connect(urls: &[String]) -> Result<Self> {
        let client = Client::default();
        let mut added = Vec::new();
        for url in urls {
            match RelayUrl::parse(url) {
                Ok(u) => {
                    client.add_relay(u).await?;
                    added.push(url.clone());
                }
                Err(e) => tracing::warn!("skipping relay {url}: {e}"),
            }
        }
        if added.is_empty() {
            bail!("no usable relays");
        }
        client.connect().and_wait(Duration::from_secs(5)).await;
        Ok(Self { client, urls: added })
    }

    pub async fn fetch(&self, filter: Filter, timeout: Duration) -> Result<Vec<Event>> {
        let events = self.client.fetch_events(filter).timeout(timeout).await?;
        Ok(events.into_iter().collect())
    }

    /// Publish to every relay. Succeeds if at least one accepted it.
    pub async fn publish(&self, event: &Event) -> Result<()> {
        let out = self.client.send_event(event).await?;
        if out.success.is_empty() {
            let reasons: Vec<String> = out.failed.iter().map(|(u, e)| format!("{u}: {e}")).collect();
            bail!("no relay accepted {}: {}", event.id, reasons.join("; "));
        }
        Ok(())
    }

    /// Subscribe, and forward matching events to a channel until it closes.
    pub async fn subscribe(&self, filter: Filter) -> Result<mpsc::UnboundedReceiver<Event>> {
        let mut notifications = self.client.notifications();
        let out = self.client.subscribe(filter).await?;
        let id = out.value;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(n) = notifications.next().await {
                if let ClientNotification::Event {
                    subscription_id,
                    event,
                    ..
                } = n
                    && subscription_id == id
                    && tx.send(*event).is_err()
                {
                    break;
                }
            }
        });
        Ok(rx)
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}

/// The user's write relays (NIP-65) and Blossom servers (kind 10063), looked up
/// on `bootstrap` relays.
pub async fn discover(bootstrap: &[String], pubkey: &PublicKey) -> Result<(Vec<String>, Vec<String>)> {
    let relays = Relays::connect(bootstrap).await?;
    let events = relays
        .fetch(
            Filter::new().author(*pubkey).kinds([
                Kind::Custom(crate::model::KIND_RELAY_LIST),
                Kind::Custom(crate::model::KIND_BLOSSOM_SERVERS),
            ]),
            Duration::from_secs(8),
        )
        .await?;
    relays.shutdown().await;
    let newest = |kind: u16| {
        events
            .iter()
            .filter(|e| e.kind.as_u16() == kind)
            .max_by_key(|e| e.created_at)
            .cloned()
    };
    let mut write = Vec::new();
    if let Some(e) = newest(crate::model::KIND_RELAY_LIST) {
        for t in e.tags.iter() {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some("r")
                && let Some(url) = s.get(1)
                && s.get(2).map(String::as_str) != Some("read")
            {
                write.push(url.clone());
            }
        }
    }
    let mut servers = Vec::new();
    if let Some(e) = newest(crate::model::KIND_BLOSSOM_SERVERS) {
        for t in e.tags.iter() {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some("server")
                && let Some(url) = s.get(1)
            {
                servers.push(url.clone());
            }
        }
    }
    Ok((write, servers))
}
