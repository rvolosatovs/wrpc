use core::pin::pin;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use async_nats::HeaderMap;
use bytes::Bytes;
use clap::Parser;
use futures::stream::select_all;
use futures::{StreamExt as _, TryStreamExt as _};
use tokio::sync::{mpsc, RwLock};
use tokio::{select, signal};
use tracing::{debug, info, instrument, warn};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use url::Url;
use wrpc_transport::{ResourceBorrow, ResourceOwn};

mod bindings {
    wit_bindgen_wrpc::generate!({
       with: {
           "wasi:keyvalue/store@0.2.0-draft": generate
       }
    });
}

use bindings::exports::wasi::keyvalue::store::{self, KeyResponse};

type Result<T> = core::result::Result<T, store::Error>;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// NATS.io URL to connect to
    #[arg(short, long, default_value = "nats://127.0.0.1:4222")]
    nats: Url,

    /// Prefix to serve `wasi:keyvalue/store` functions on
    #[arg(default_value = "rust")]
    prefix: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(tracing_subscriber::fmt::layer().compact().without_time())
        .init();

    let Args { nats, prefix } = Args::parse();

    let nats = connect(nats)
        .await
        .context("failed to connect to NATS.io")?;
    let invocations = bindings::serve(
        &wrpc_transport_nats::Client::new(nats, prefix, None),
        Server::default(),
    )
    .await
    .context("failed to serve `wasi:keyvalue/store`")?;
    // NOTE: This will conflate all invocation streams into a single stream via `futures::stream::SelectAll`,
    // to customize this, iterate over the returned `invocations` and set up custom handling per export
    let mut invocations = select_all(invocations.into_iter().map(
        |(instance, name, invocations)| {
            invocations
                .try_buffer_unordered(16) // handle up to 16 invocations concurrently
                .map(move |res| (instance, name, res))
        },
    ));
    let shutdown = signal::ctrl_c();
    let mut shutdown = pin!(shutdown);
    loop {
        select! {
            Some((instance, name, res)) = invocations.next() => {
                match res {
                    Ok(()) => {
                        info!(instance, name, "invocation successfully handled");
                    }
                    Err(err) => {
                        warn!(?err, instance, name, "failed to accept invocation");
                    }
                }
            }
            res = &mut shutdown => {
                return res.context("failed to listen for ^C")
            }
        }
    }
}

/// Connect to NATS.io server and ensure that the connection is fully established before
/// returning the resulting [`async_nats::Client`]
async fn connect(url: Url) -> anyhow::Result<async_nats::Client> {
    let (conn_tx, mut conn_rx) = mpsc::channel(1);
    let client = async_nats::connect_with_options(
        String::from(url),
        async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .event_callback(move |event| {
                let conn_tx = conn_tx.clone();
                async move {
                    if let async_nats::Event::Connected = event {
                        conn_tx
                            .send(())
                            .await
                            .expect("failed to send NATS.io server connection notification");
                    }
                }
            }),
    )
    .await
    .context("failed to connect to NATS.io server")?;
    conn_rx
        .recv()
        .await
        .context("failed to await NATS.io server connection to be established")?;
    Ok(client)
}

type Bucket = Arc<RwLock<HashMap<String, Bytes>>>;

#[derive(Clone, Debug, Default)]
struct Server(Arc<RwLock<HashMap<Bytes, Bucket>>>);

impl Server {
    async fn bucket(&self, bucket: impl AsRef<[u8]>) -> Result<Bucket> {
        debug!("looking up bucket");
        let store = self.0.read().await;
        store
            .get(bucket.as_ref())
            .ok_or(store::Error::NoSuchStore)
            .cloned()
    }
}

impl bindings::exports::wasi::keyvalue::store::HandlerBucket<Option<HeaderMap>> for Server {

    async fn get(
        &self,
        _cx: Option<HeaderMap>,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
    ) -> anyhow::Result<Result<Option<Bytes>>> {
        let bucket = self.bucket(bucket).await?;
        let bucket = bucket.read().await;
        Ok(Ok(bucket.get(&key).cloned()))
    }


    async fn set(
        &self,
        _cx: Option<HeaderMap>,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
        value: Bytes,
    ) -> anyhow::Result<Result<()>> {
        let bucket = self.bucket(bucket).await?;
        let mut bucket = bucket.write().await;
        bucket.insert(key, value);
        Ok(Ok(()))
    }


    async fn delete(
        &self,
        _cx: Option<HeaderMap>,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
    ) -> anyhow::Result<Result<()>> {
        let bucket = self.bucket(bucket).await?;
        let mut bucket = bucket.write().await;
        bucket.remove(&key);
        Ok(Ok(()))
    }


    async fn exists(
        &self,
        _cx: Option<HeaderMap>,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
    ) -> anyhow::Result<Result<bool>> {
        let bucket = self.bucket(bucket).await?;
        let bucket = bucket.read().await;
        Ok(Ok(bucket.contains_key(&key)))
    }


    async fn list_keys(
        &self,
        _cx: Option<HeaderMap>,
        bucket: ResourceBorrow<store::Bucket>,
        cursor: Option<u64>,
    ) -> anyhow::Result<Result<KeyResponse>> {
        let bucket = self.bucket(bucket).await?;
        let bucket = bucket.read().await;
        let bucket = bucket.keys();
        let keys = if let Some(cursor) = cursor {
            let cursor =
                usize::try_from(cursor).map_err(|err| store::Error::Other(err.to_string()))?;
            bucket.skip(cursor).cloned().collect()
        } else {
            bucket.cloned().collect()
        };
        Ok(Ok(KeyResponse { keys, cursor: None }))
    }
}

impl bindings::exports::wasi::keyvalue::store::Handler<Option<HeaderMap>> for Server {
    // NOTE: Resource handle returned is just the `identifier` itself

    async fn open(
        &self,
        _cx: Option<HeaderMap>,
        identifier: String,
    ) -> anyhow::Result<Result<ResourceOwn<store::Bucket>>> {
        let identifier = Bytes::from(identifier);
        {
            // first, optimistically try read-only lock
            let store = self.0.read().await;
            if store.contains_key(&identifier) {
                return Ok(Ok(ResourceOwn::from(identifier)));
            }
        }
        let mut store = self.0.write().await;
        store.entry(identifier.clone()).or_default();
        Ok(Ok(ResourceOwn::from(identifier)))
    }
}
