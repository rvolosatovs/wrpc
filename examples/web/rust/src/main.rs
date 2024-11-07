use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use core::pin::pin;
use core::time::Duration;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use axum::http::header::CONTENT_TYPE;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use clap::Parser;
use futures::stream::select_all;
use futures::StreamExt as _;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::version::TLS13;
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tokio::{select, signal};
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, instrument, trace, warn};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use url::Url;
use uuid::Uuid;
use wrpc_transport::{ResourceBorrow, ResourceOwn};
use wrpc_wasi_keyvalue::exports::wasi::keyvalue::store;
use wtransport::tls::{Sha256DigestFmt, WEBTRANSPORT_ALPN};
use wtransport::{Endpoint, Identity, ServerConfig};

pub type Result<T, E = store::Error> = core::result::Result<T, E>;

/// This completely disables server TLS certificate verification.
// NOTE: Do not use in production setting!
#[derive(Debug)]
struct Insecure;

impl ServerCertVerifier for Insecure {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ECDSA_NISTP256_SHA256]
    }
}

#[derive(Clone, Debug)]
enum Bucket {
    Mem(ResourceOwn<store::Bucket>),
    Redis(ResourceOwn<store::Bucket>),
    Nats(
        ResourceOwn<wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket>,
        wrpc_transport_nats::Client,
    ),
    Quic(
        ResourceOwn<wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket>,
        wrpc_transport_quic::Client,
    ),
    Tcp(
        ResourceOwn<wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket>,
        wrpc_transport::tcp::Client<SocketAddr>,
    ),
    #[cfg(unix)]
    Unix(
        ResourceOwn<wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket>,
        wrpc_transport::unix::Client<std::path::PathBuf>,
    ),
    Web(
        ResourceOwn<wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket>,
        wrpc_transport_web::Client,
    ),
}

#[derive(Clone, Default)]
struct Handler {
    buckets: Arc<RwLock<HashMap<Bytes, Bucket>>>,
    mem: wrpc_wasi_keyvalue_mem::Handler,
    redis: wrpc_wasi_keyvalue_redis::Handler,
}

impl Handler {
    async fn bucket(&self, bucket: impl AsRef<[u8]>) -> Result<Bucket> {
        trace!("looking up bucket");
        let store = self.buckets.read().await;
        store
            .get(bucket.as_ref())
            .ok_or(store::Error::NoSuchStore)
            .cloned()
    }
}

fn parse_addr(url: &Url, default_port: u16) -> Result<SocketAddr> {
    let auth = url.authority();
    if url.port().is_some() {
        match auth.parse().context("failed to parse IP address") {
            Ok(addr) => Ok(addr),
            Err(err) => Err(store::Error::Other(format!("{err:#}"))),
        }
    } else {
        match auth.parse().context("failed to parse socket address") {
            Ok(ip) => Ok(SocketAddr::new(ip, default_port)),
            Err(err) => Err(store::Error::Other(format!("{err:#}"))),
        }
    }
}

// TODO: Allow TLS configuration via runtime flags
// TODO: Support WebPKI
fn client_tls_config() -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_protocol_versions(&[&TLS13])
        .dangerous()
        // NOTE: Do not do this in production!
        .with_custom_certificate_verifier(Arc::new(Insecure))
        .with_no_client_auth()
}

impl<C: Send + Sync> store::Handler<C> for Handler {
    #[instrument(level = "trace", skip(self, cx), ret(level = "trace"))]
    async fn open(
        &self,
        cx: C,
        identifier: String,
    ) -> anyhow::Result<Result<ResourceOwn<store::Bucket>>> {
        if identifier.is_empty() {
            let bucket = match self.mem.open(cx, identifier).await? {
                Ok(bucket) => bucket,
                Err(err) => return Ok(Err(err)),
            };
            let id = Uuid::now_v7();
            let id = Bytes::copy_from_slice(id.as_bytes());
            let mut buckets = self.buckets.write().await;
            buckets.insert(id.clone(), Bucket::Mem(bucket));
            return Ok(Ok(ResourceOwn::from(id)));
        }
        let (url, suffix) = identifier.split_once(';').unwrap_or((&identifier, ""));
        let url = match Url::parse(url) {
            Ok(url) => url,
            Err(err) => return Ok(Err(store::Error::Other(err.to_string()))),
        };
        let bucket = match url.scheme() {
            "redis" | "rediss" | "redis+sentinel" | "rediss+sentinel" => {
                match self.redis.open(cx, identifier).await? {
                    Ok(bucket) => Bucket::Redis(bucket),
                    Err(err) => return Ok(Err(err)),
                }
            }
            "wrpc+nats" => {
                let nats = match async_nats::connect_with_options(
                    url.authority(),
                    async_nats::ConnectOptions::new().retry_on_initial_connect(),
                )
                .await
                .context("failed to connect to NATS.io server")
                {
                    Ok(nats) => nats,
                    Err(err) => return Ok(Err(store::Error::Other(format!("{err:#}")))),
                };
                let prefix = url.path();
                let prefix = prefix.strip_prefix('/').unwrap_or(prefix);
                let wrpc = match wrpc_transport_nats::Client::new(nats, prefix, None)
                    .await
                    .context("failed to construct wRPC client")
                {
                    Ok(wrpc) => wrpc,
                    Err(err) => return Ok(Err(store::Error::Other(format!("{err:#}")))),
                };
                match wrpc_wasi_keyvalue::wasi::keyvalue::store::open(&wrpc, None, suffix).await? {
                    Ok(bucket) => Bucket::Nats(bucket, wrpc),
                    Err(err) => return Ok(Err(err.into())),
                }
            }
            "wrpc+quic" => {
                let addr = match parse_addr(&url, 443) {
                    Ok(addr) => addr,
                    Err(err) => return Ok(Err(err)),
                };
                let mut ep = match quinn::Endpoint::client((Ipv6Addr::UNSPECIFIED, 0).into())
                    .context("failed to create QUIC endpoint")
                {
                    Ok(ep) => ep,
                    Err(err) => return Ok(Err(store::Error::Other(format!("{err:#}")))),
                };
                let san = url.path();
                let mut san = san.strip_prefix('/').unwrap_or(san);
                if san.is_empty() {
                    san = "localhost"
                }
                let conf: QuicClientConfig = client_tls_config()
                    .try_into()
                    .context("failed to convert rustls client config to QUIC client config")?;
                ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(conf)));
                let conn = match ep
                    .connect(addr, san)
                    .context("failed to connect using QUIC")
                {
                    Ok(ep) => ep,
                    Err(err) => return Ok(Err(store::Error::Other(format!("{err:#}")))),
                };
                let conn = match conn.await.context("failed to establish QUIC connection") {
                    Ok(ep) => ep,
                    Err(err) => return Ok(Err(store::Error::Other(format!("{err:#}")))),
                };
                let wrpc = wrpc_transport_quic::Client::from(conn);
                match wrpc_wasi_keyvalue::wasi::keyvalue::store::open(&wrpc, (), suffix).await? {
                    Ok(bucket) => Bucket::Quic(bucket, wrpc),
                    Err(err) => return Ok(Err(err.into())),
                }
            }
            "wrpc+tcp" => {
                let addr = match parse_addr(&url, 7761) {
                    Ok(addr) => addr,
                    Err(err) => return Ok(Err(err)),
                };
                let wrpc = wrpc_transport::frame::tcp::Client::from(addr);
                match wrpc_wasi_keyvalue::wasi::keyvalue::store::open(&wrpc, (), suffix).await? {
                    Ok(bucket) => Bucket::Tcp(bucket, wrpc),
                    Err(err) => return Ok(Err(err.into())),
                }
            }
            #[cfg(unix)]
            "wrpc+unix" => {
                let Ok(path) = url.to_file_path() else {
                    return Ok(Err(store::Error::Other(
                        "failed to get filesystem path from URL".to_string(),
                    )));
                };
                let wrpc = wrpc_transport::frame::unix::Client::from(path);
                match wrpc_wasi_keyvalue::wasi::keyvalue::store::open(&wrpc, (), suffix).await? {
                    Ok(bucket) => Bucket::Unix(bucket, wrpc),
                    Err(err) => return Ok(Err(err.into())),
                }
            }
            "wrpc+web" => {
                let mut conf = client_tls_config();
                conf.alpn_protocols.push(WEBTRANSPORT_ALPN.to_vec());
                let ep = match wtransport::Endpoint::client(
                    wtransport::ClientConfig::builder()
                        .with_bind_default()
                        .with_custom_tls(conf)
                        .build(),
                )
                .context("failed to create WebTransport endpoint")
                {
                    Ok(ep) => ep,
                    Err(err) => return Ok(Err(store::Error::Other(format!("{err:#}")))),
                };
                let conn = match ep
                    .connect(format!("https://{}", url.authority()))
                    .await
                    .context("failed to establish WebTransport connection")
                {
                    Ok(ep) => ep,
                    Err(err) => return Ok(Err(store::Error::Other(format!("{err:#}")))),
                };
                let wrpc = wrpc_transport_web::Client::from(conn);
                match wrpc_wasi_keyvalue::wasi::keyvalue::store::open(&wrpc, (), suffix).await? {
                    Ok(bucket) => Bucket::Web(bucket, wrpc),
                    Err(err) => return Ok(Err(err.into())),
                }
            }
            scheme => {
                return Ok(Err(store::Error::Other(format!(
                    "unsupported scheme: {scheme}"
                ))))
            }
        };
        let id = Uuid::now_v7();
        let id = Bytes::copy_from_slice(id.as_bytes());
        let mut buckets = self.buckets.write().await;
        buckets.insert(id.clone(), bucket);
        Ok(Ok(ResourceOwn::from(id)))
    }
}

impl<C: Send + Sync> store::HandlerBucket<C> for Handler {
    #[instrument(level = "trace", skip(self, cx), ret(level = "trace"))]
    async fn get(
        &self,
        cx: C,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
    ) -> anyhow::Result<Result<Option<Bytes>>> {
        let res = match self.bucket(bucket).await? {
            Bucket::Mem(bucket) => return self.mem.get(cx, bucket.as_borrow(), key).await,
            Bucket::Redis(bucket) => return self.redis.get(cx, bucket.as_borrow(), key).await,
            Bucket::Nats(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::get(
                    &wrpc,
                    None,
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Quic(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::get(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Tcp(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::get(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            #[cfg(unix)]
            Bucket::Unix(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::get(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Web(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::get(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
        };
        match res {
            Ok(res) => Ok(Ok(res)),
            Err(err) => Ok(Err(err.into())),
        }
    }

    #[instrument(level = "trace", skip(self, cx), ret(level = "trace"))]
    async fn set(
        &self,
        cx: C,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
        value: Bytes,
    ) -> anyhow::Result<Result<()>> {
        let res = match self.bucket(bucket).await? {
            Bucket::Mem(bucket) => return self.mem.set(cx, bucket.as_borrow(), key, value).await,
            Bucket::Redis(bucket) => {
                return self.redis.set(cx, bucket.as_borrow(), key, value).await
            }
            Bucket::Nats(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::set(
                    &wrpc,
                    None,
                    &bucket.as_borrow(),
                    &key,
                    &value,
                )
                .await?
            }
            Bucket::Quic(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::set(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                    &value,
                )
                .await?
            }
            Bucket::Tcp(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::set(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                    &value,
                )
                .await?
            }
            #[cfg(unix)]
            Bucket::Unix(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::set(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                    &value,
                )
                .await?
            }
            Bucket::Web(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::set(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                    &value,
                )
                .await?
            }
        };
        match res {
            Ok(()) => Ok(Ok(())),
            Err(err) => Ok(Err(err.into())),
        }
    }

    #[instrument(level = "trace", skip(self, cx), ret(level = "trace"))]
    async fn delete(
        &self,
        cx: C,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
    ) -> anyhow::Result<Result<()>> {
        let res = match self.bucket(bucket).await? {
            Bucket::Mem(bucket) => return self.mem.delete(cx, bucket.as_borrow(), key).await,
            Bucket::Redis(bucket) => return self.redis.delete(cx, bucket.as_borrow(), key).await,
            Bucket::Nats(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::delete(
                    &wrpc,
                    None,
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Quic(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::delete(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Tcp(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::delete(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            #[cfg(unix)]
            Bucket::Unix(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::delete(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Web(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::delete(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
        };
        match res {
            Ok(()) => Ok(Ok(())),
            Err(err) => Ok(Err(err.into())),
        }
    }

    #[instrument(level = "trace", skip(self, cx), ret(level = "trace"))]
    async fn exists(
        &self,
        cx: C,
        bucket: ResourceBorrow<store::Bucket>,
        key: String,
    ) -> anyhow::Result<Result<bool>> {
        let res = match self.bucket(bucket).await? {
            Bucket::Mem(bucket) => return self.mem.exists(cx, bucket.as_borrow(), key).await,
            Bucket::Redis(bucket) => return self.redis.exists(cx, bucket.as_borrow(), key).await,
            Bucket::Nats(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::exists(
                    &wrpc,
                    None,
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Quic(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::exists(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Tcp(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::exists(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            #[cfg(unix)]
            Bucket::Unix(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::exists(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
            Bucket::Web(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::exists(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    &key,
                )
                .await?
            }
        };
        match res {
            Ok(res) => Ok(Ok(res)),
            Err(err) => Ok(Err(err.into())),
        }
    }

    #[instrument(level = "trace", skip(self, cx), ret(level = "trace"))]
    async fn list_keys(
        &self,
        cx: C,
        bucket: ResourceBorrow<store::Bucket>,
        cursor: Option<String>,
    ) -> anyhow::Result<Result<store::KeyResponse>> {
        let res = match self.bucket(bucket).await? {
            Bucket::Mem(bucket) => return self.mem.list_keys(cx, bucket.as_borrow(), cursor).await,
            Bucket::Redis(bucket) => {
                return self.redis.list_keys(cx, bucket.as_borrow(), cursor).await
            }
            Bucket::Nats(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::list_keys(
                    &wrpc,
                    None,
                    &bucket.as_borrow(),
                    cursor.as_deref(),
                )
                .await?
            }
            Bucket::Quic(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::list_keys(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    cursor.as_deref(),
                )
                .await?
            }
            Bucket::Tcp(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::list_keys(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    cursor.as_deref(),
                )
                .await?
            }
            #[cfg(unix)]
            Bucket::Unix(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::list_keys(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    cursor.as_deref(),
                )
                .await?
            }
            Bucket::Web(bucket, wrpc) => {
                wrpc_wasi_keyvalue::wasi::keyvalue::store::Bucket::list_keys(
                    &wrpc,
                    (),
                    &bucket.as_borrow(),
                    cursor.as_deref(),
                )
                .await?
            }
        };
        match res {
            Ok(res) => Ok(Ok(res.into())),
            Err(err) => Ok(Err(err.into())),
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Address to serve web interface on
    #[arg(default_value = "[::1]:8080")]
    addr: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact().without_time())
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Args { addr } = Args::parse();

    let lis = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind TCP listener on `{addr}`"))?;

    let id = Identity::self_signed(["localhost", "127.0.0.1", "::1"])
        .context("failed to generate server certificate")?;
    let digest = id.certificate_chain().as_slice()[0]
        .hash()
        .fmt(Sha256DigestFmt::BytesArray);

    let ep = Endpoint::server(
        ServerConfig::builder()
            .with_bind_config(wtransport::config::IpBindConfig::LocalV4, 5051)
            .with_identity(id)
            .keep_alive_interval(Some(Duration::from_secs(3)))
            .build(),
    )
    .context("failed to create server endpoint")?;
    let ep_addr = ep
        .local_addr()
        .context("failed to query WebTransport socket address")?;
    let port = ep_addr.port();

    let index = get(Html(include_str!("../../ui/index.html")));
    let http = axum::serve(
        lis,
        Router::new()
            .route("/", index.clone())
            .route(
                "/consts.js",
                get((
                    [(CONTENT_TYPE, "application/javascript")],
                    format!(
                        r#"
export const CERT_DIGEST = new Uint8Array({digest});
export const PORT = "{port}"
"#
                    ),
                )),
            )
            .fallback(index)
            .layer(TraceLayer::new_for_http()),
    );

    let srv = Arc::new(wrpc_transport_web::Server::new());
    let webt = tokio::spawn({
        let mut tasks = JoinSet::<anyhow::Result<()>>::new();
        let srv = Arc::clone(&srv);
        async move {
            loop {
                select! {
                    conn = ep.accept() => {
                        let srv = Arc::clone(&srv);
                        tasks.spawn(async move {
                            let req = conn
                                .await
                                .context("failed to accept WebTransport connection")?;
                            let conn = req
                                .accept()
                                .await
                                .context("failed to establish WebTransport connection")?;
                            let wrpc = wrpc_transport_web::Client::from(conn);
                            loop {
                                srv.accept(&wrpc)
                                    .await
                                    .context("failed to accept wRPC connection")?;
                            }
                        });
                    }
                    Some(res) = tasks.join_next() => {
                        match res {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                warn!(?err, "failed to serve connection")
                            }
                            Err(err) => {
                                error!(?err, "failed to join task")
                            }
                        }
                    }
                }
            }
        }
    });

    let invocations = wrpc_wasi_keyvalue::exports::wasi::keyvalue::store::serve_interface(
        srv.as_ref(),
        Handler::default(),
    )
    .await
    .context("failed to serve `wasi:keyvalue`")?;
    let mut invocations = select_all(
        invocations
            .into_iter()
            .map(|(instance, name, invocations)| invocations.map(move |res| (instance, name, res))),
    );
    let wrpc = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            select! {
                Some((instance, name, res)) = invocations.next() => {
                    match res {
                        Ok(fut) => {
                            debug!(instance, name, "invocation accepted");
                            tasks.spawn(async move {
                                if let Err(err) = fut.await {
                                    warn!(?err, "failed to handle invocation");
                                } else {
                                    info!(instance, name, "invocation successfully handled");
                                }
                            });
                        }
                        Err(err) => {
                            warn!(?err, instance, name, "failed to accept invocation");
                        }
                    }
                }
                Some(res) = tasks.join_next() => {
                    if let Err(err) = res {
                        error!(?err, "failed to join task")
                    }
                }
                else => {
                    return
                }
            }
        }
    });

    let shutdown = signal::ctrl_c();
    let mut shutdown = pin!(shutdown);
    info!("serving HTTP on: http://{addr}");
    select! {
        res = http => {
            trace!("HTTP serving stopped");
            res.context("failed to serve HTTP")?;
        }
        res = webt => {
            trace!("WebTransport serving task stopped");
            res.context("failed to serve WebTransport")?;
        }
        res = wrpc => {
            trace!("wRPC serving task stopped");
            res.context("failed to serve wRPC invocations")?;
        }
        res = &mut shutdown => {
            trace!("^C received");
            res.context("failed to listen for ^C")?;
        }
    }
    Ok(())
}
