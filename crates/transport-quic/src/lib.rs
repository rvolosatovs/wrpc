use core::fmt::Display;
use core::future::Future as _;
use core::net::SocketAddr;
use core::pin::{pin, Pin};
use core::task::{ready, Context, Poll};
use core::{mem, str};

use std::collections::{hash_map, HashMap};
use std::sync::Arc;

use anyhow::{bail, ensure, Context as _};
use bytes::{Buf as _, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use pin_project_lite::pin_project;
use quinn::crypto::rustls::HandshakeData;
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, SendStream, VarInt};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinSet;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::codec::Encoder;
use tracing::{instrument, trace, warn, Instrument as _, Span};
use wasm_tokio::{AsyncReadLeb128 as _, Leb128Encoder};

pub const PROTOCOL: u8 = 0;

fn san(instance: &str, func: &str) -> String {
    let mut s = String::with_capacity(
        13_usize // ".server.wrpc" + '.'
            .saturating_add(instance.len())
            .saturating_add(func.len()),
    );
    for p in func.split('.').rev() {
        s.push_str(p);
    }
    s.push('.');
    s.push_str(&instance.replace(':', "_").replace('/', "__"));
    s.push_str(".server.wrpc");
    s
}

#[derive(Default)]
pub enum IndexTree {
    #[default]
    Empty,
    Leaf {
        tx: Option<oneshot::Sender<RecvStream>>,
        rx: Option<oneshot::Receiver<RecvStream>>,
    },
    IndexNode {
        tx: Option<oneshot::Sender<RecvStream>>,
        rx: Option<oneshot::Receiver<RecvStream>>,
        nested: Vec<Option<IndexTree>>,
    },
    // TODO: Add partially-indexed `WildcardIndexNode`
    WildcardNode {
        tx: Option<oneshot::Sender<RecvStream>>,
        rx: Option<oneshot::Receiver<RecvStream>>,
        nested: Option<Box<IndexTree>>,
    },
}

impl<'a>
    From<(
        &'a [Option<usize>],
        Option<oneshot::Sender<RecvStream>>,
        Option<oneshot::Receiver<RecvStream>>,
    )> for IndexTree
{
    fn from(
        (path, tx, rx): (
            &'a [Option<usize>],
            Option<oneshot::Sender<RecvStream>>,
            Option<oneshot::Receiver<RecvStream>>,
        ),
    ) -> Self {
        match path {
            [] => Self::Leaf { tx, rx },
            [None, path @ ..] => Self::WildcardNode {
                tx: None,
                rx: None,
                nested: Some(Box::new(Self::from((path, tx, rx)))),
            },
            [Some(i), path @ ..] => Self::IndexNode {
                tx: None,
                rx: None,
                nested: {
                    let n = i.saturating_add(1);
                    let mut nested = Vec::with_capacity(n);
                    nested.resize_with(n, Option::default);
                    nested[*i] = Some(Self::from((path, tx, rx)));
                    nested
                },
            },
        }
    }
}

impl<'a>
    From<(
        &'a [Option<usize>],
        oneshot::Sender<RecvStream>,
        oneshot::Receiver<RecvStream>,
    )> for IndexTree
{
    fn from(
        (path, tx, rx): (
            &'a [Option<usize>],
            oneshot::Sender<RecvStream>,
            oneshot::Receiver<RecvStream>,
        ),
    ) -> Self {
        Self::from((path, Some(tx), Some(rx)))
    }
}

impl<'a> From<(&'a [Option<usize>], oneshot::Sender<RecvStream>)> for IndexTree {
    fn from((path, tx): (&'a [Option<usize>], oneshot::Sender<RecvStream>)) -> Self {
        Self::from((path, Some(tx), None))
    }
}

impl<P: AsRef<[Option<usize>]>> FromIterator<P> for IndexTree {
    fn from_iter<T: IntoIterator<Item = P>>(iter: T) -> Self {
        let mut root = Self::Empty;
        for path in iter {
            let (tx, rx) = oneshot::channel();
            if !root.insert(path.as_ref(), Some(tx), Some(rx)) {
                return Self::Empty;
            }
        }
        root
    }
}

impl IndexTree {

    fn take_rx(&mut self, path: &[usize]) -> Option<oneshot::Receiver<RecvStream>> {
        let Some((i, path)) = path.split_first() else {
            return match self {
                Self::Empty => None,
                Self::Leaf { rx, .. } => rx.take(),
                Self::IndexNode { tx, rx, nested } => {
                    let rx = rx.take();
                    if nested.is_empty() && tx.is_none() {
                        *self = Self::Empty;
                    }
                    rx
                }
                Self::WildcardNode { tx, rx, nested } => {
                    let rx = rx.take();
                    if nested.is_none() && tx.is_none() {
                        *self = Self::Empty;
                    }
                    rx
                }
            };
        };
        match self {
            Self::Empty | Self::Leaf { .. } | Self::WildcardNode { .. } => None,
            Self::IndexNode { ref mut nested, .. } => nested
                .get_mut(*i)
                .and_then(|nested| nested.as_mut().and_then(|nested| nested.take_rx(path))),
            // TODO: Demux the subscription
            //Self::WildcardNode { ref mut nested, .. } => {
            //    nested.as_mut().and_then(|nested| nested.take(path))
            //}
        }
    }


    fn take_tx(&mut self, path: &[usize]) -> Option<oneshot::Sender<RecvStream>> {
        let Some((i, path)) = path.split_first() else {
            return match self {
                Self::Empty => None,
                Self::Leaf { tx, .. } => tx.take(),
                Self::IndexNode { tx, rx, nested } => {
                    let tx = tx.take();
                    if nested.is_empty() && rx.is_none() {
                        *self = Self::Empty;
                    }
                    tx
                }
                Self::WildcardNode { tx, rx, nested } => {
                    let tx = tx.take();
                    if nested.is_none() && rx.is_none() {
                        *self = Self::Empty;
                    }
                    tx
                }
            };
        };
        match self {
            Self::Empty | Self::Leaf { .. } | Self::WildcardNode { .. } => None,
            Self::IndexNode { ref mut nested, .. } => nested
                .get_mut(*i)
                .and_then(|nested| nested.as_mut().and_then(|nested| nested.take_tx(path))),
            // TODO: Demux the subscription
            //Self::WildcardNode { ref mut nested, .. } => {
            //    nested.as_mut().and_then(|nested| nested.take(path))
            //}
        }
    }

    /// Inserts `sender` and `receiver` under a `path` - returns `false` if it failed and `true` if it succeeded.
    /// Tree state after `false` is returned is undefined

    fn insert(
        &mut self,
        path: &[Option<usize>],
        sender: Option<oneshot::Sender<RecvStream>>,
        receiver: Option<oneshot::Receiver<RecvStream>>,
    ) -> bool {
        match self {
            Self::Empty => {
                *self = Self::from((path, sender, receiver));
                true
            }
            Self::Leaf { .. } => {
                let Some((i, path)) = path.split_first() else {
                    return false;
                };
                let Self::Leaf { tx, rx } = mem::take(self) else {
                    return false;
                };
                if let Some(i) = i {
                    let n = i.saturating_add(1);
                    let mut nested = Vec::with_capacity(n);
                    nested.resize_with(n, Option::default);
                    nested[*i] = Some(Self::from((path, sender, receiver)));
                    *self = Self::IndexNode { tx, rx, nested };
                } else {
                    *self = Self::WildcardNode {
                        tx,
                        rx,
                        nested: Some(Box::new(Self::from((path, sender, receiver)))),
                    };
                }
                true
            }
            Self::IndexNode {
                ref mut tx,
                ref mut rx,
                ref mut nested,
            } => match (&tx, &rx, path) {
                (None, None, []) => {
                    *tx = sender;
                    *rx = receiver;
                    true
                }
                (_, _, [Some(i), path @ ..]) => {
                    let cap = i.saturating_add(1);
                    if nested.len() < cap {
                        nested.resize_with(cap, Option::default);
                    }
                    let nested = &mut nested[*i];
                    if let Some(nested) = nested {
                        nested.insert(path, sender, receiver)
                    } else {
                        *nested = Some(Self::from((path, sender, receiver)));
                        true
                    }
                }
                _ => false,
            },
            Self::WildcardNode {
                ref mut tx,
                ref mut rx,
                ref mut nested,
            } => match (&tx, &rx, path) {
                (None, None, []) => {
                    *tx = sender;
                    *rx = receiver;
                    true
                }
                (_, _, [None, path @ ..]) => {
                    if let Some(nested) = nested {
                        nested.insert(path, sender, receiver)
                    } else {
                        *nested = Some(Box::new(Self::from((path, sender, receiver))));
                        true
                    }
                }
                _ => false,
            },
        }
    }
}

#[derive(Default)]
pub struct Server(Mutex<HashMap<String, mpsc::Sender<Connection>>>);

#[derive(Debug)]
pub enum AcceptError {
    Connection(ConnectionError),
    InvalidData,
    ServerNameMissing,
    UnhandledName(String),
    SendError(mpsc::error::SendError<Connection>),
}

impl Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::Connection(err) => err.fmt(f),
            AcceptError::InvalidData => write!(f, "unexpected handshake data type"),
            AcceptError::ServerNameMissing => write!(f, "server name missing in handshake data"),
            AcceptError::UnhandledName(name) => {
                write!(f, "SAN `{name}` does not have a handler registered")
            }
            AcceptError::SendError(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for AcceptError {}

impl Server {
    /// Accept a connection on an endpoint.
    /// Returns `Ok(false)` if the endpoint is closed.
    ///
    /// # Errors
    ///
    /// Returns an error if accepting the connection has failed
    pub async fn accept(&self, endpoint: &quinn::Endpoint) -> Result<bool, AcceptError> {
        let Some(conn) = endpoint.accept().await else {
            return Ok(false);
        };
        let mut conn = conn.accept().map_err(AcceptError::Connection)?;
        let data = conn
            .handshake_data()
            .await
            .map_err(AcceptError::Connection)?;
        let data = data
            .downcast::<HandshakeData>()
            .map_err(|_| AcceptError::InvalidData)?;
        let name = data.server_name.ok_or(AcceptError::ServerNameMissing)?;
        let tx = self.0.lock().await;
        let tx = tx
            .get(&name)
            .ok_or_else(|| AcceptError::UnhandledName(name))?;
        let conn = conn.await.map_err(AcceptError::Connection)?;
        tx.send(conn).await.map_err(AcceptError::SendError)?;
        Ok(true)
    }
}

pub struct Client {
    endpoint: Endpoint,
    addr: SocketAddr,
}

impl Client {
    pub fn new(endpoint: Endpoint, addr: impl Into<SocketAddr>) -> Self {
        Self {
            endpoint,
            addr: addr.into(),
        }
    }
}

pin_project! {
    #[project = IncomingProj]
    pub enum Incoming {
        Accepting {
            index: Arc<std::sync::Mutex<IndexTree>>,
            path: Arc<[usize]>,
            #[pin]
            rx: oneshot::Receiver<RecvStream>,
            io: Arc<JoinSet<std::io::Result<()>>>,
        },
        Active {
            index: Arc<std::sync::Mutex<IndexTree>>,
            path: Arc<[usize]>,
            #[pin]
            rx: RecvStream,
            io: Arc<JoinSet<std::io::Result<()>>>,
        },
    }
}

impl wrpc_transport::Index<Self> for Incoming {

    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        match self {
            Self::Accepting {
                index,
                path: base,
                io,
                ..
            }
            | Self::Active {
                index,
                path: base,
                io,
                ..
            } => {
                let path = if base.is_empty() {
                    Arc::from(path)
                } else {
                    Arc::from([base.as_ref(), path].concat())
                };
                trace!("locking index tree");
                let mut lock = index.lock().map_err(|err| {
                    std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
                })?;
                trace!("taking index subscription");
                let rx = lock.take_rx(&path).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("`{path:?}` subscription not found"),
                    )
                })?;
                Ok(Self::Accepting {
                    index: Arc::clone(index),
                    path,
                    rx,
                    io: Arc::clone(io),
                })
            }
        }
    }
}

impl AsyncRead for Incoming {

    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.as_mut().project() {
            IncomingProj::Accepting {
                index,
                path,
                mut rx,
                io,
            } => {
                trace!(?path, "polling channel");
                let rx = ready!(rx.as_mut().poll(cx))
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err))?;
                *self = Self::Active {
                    index: Arc::clone(index),
                    path: Arc::clone(path),
                    rx,
                    io: Arc::clone(io),
                };
                self.poll_read(cx, buf)
            }
            IncomingProj::Active { rx, path, .. } => {
                trace!(?path, "reading buffer");
                ready!(AsyncRead::poll_read(rx, cx, buf))?;
                trace!(buf = ?buf.filled(), "read from buffer");
                Poll::Ready(Ok(()))
            }
        }
    }
}

pin_project! {
    #[project = OutgoingProj]
    pub enum Outgoing {
        Opening {
            header: Bytes,
            path: Arc<[usize]>,
            #[pin]
            conn: Connection,
            io: Arc<JoinSet<std::io::Result<()>>>,
        },
        Active {
            header: Bytes,
            path: Arc<[usize]>,
            #[pin]
            conn: Connection,
            #[pin]
            tx: SendStream,
            io: Arc<JoinSet<std::io::Result<()>>>,
        },
    }
}

impl wrpc_transport::Index<Self> for Outgoing {

    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        let mut header = BytesMut::with_capacity(path.len().saturating_add(5));
        let depth = path.len();
        let n = u32::try_from(depth)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        trace!(n, "encoding path length");
        Leb128Encoder.encode(n, &mut header)?;
        for p in path {
            let p = u32::try_from(*p)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            trace!(p, "encoding path element");
            Leb128Encoder.encode(p, &mut header)?;
        }
        match self {
            Self::Opening {
                path: base,
                conn,
                io,
                ..
            }
            | Self::Active {
                path: base,
                conn,
                io,
                ..
            } => Ok(Self::Opening {
                header: header.freeze(),
                path: Arc::from([base, path].concat()),
                conn: conn.clone(),
                io: Arc::clone(io),
            }),
        }
    }
}

fn poll_write_header(
    cx: &mut Context<'_>,
    tx: &mut Pin<&mut SendStream>,
    header: &mut Bytes,
) -> Poll<std::io::Result<()>> {
    while !header.is_empty() {
        trace!(header = format!("{header:02x?}"), "writing header");
        let n = ready!(AsyncWrite::poll_write(tx.as_mut(), cx, header))?;
        if n < header.len() {
            header.advance(n);
        } else {
            *header = Bytes::default();
        }
    }
    Poll::Ready(Ok(()))
}

impl Outgoing {

    fn poll_flush_header(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.as_mut().project() {
            OutgoingProj::Opening {
                path,
                conn,
                header,
                io,
            } => {
                trace!(?path, "opening connection");
                let tx = ready!(pin!(conn.open_uni()).poll(cx)).map_err(std::io::Error::from)?;
                *self = Self::Active {
                    header: header.clone(),
                    path: Arc::clone(path),
                    conn: conn.clone(),
                    tx,
                    io: Arc::clone(io),
                };
                self.poll_flush_header(cx)
            }
            OutgoingProj::Active {
                ref mut tx, header, ..
            } => poll_write_header(cx, tx, header),
        }
    }
}

fn corrupted_memory_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, "corrupted memory state")
}

impl AsyncWrite for Outgoing {

    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        ready!(self.as_mut().poll_flush_header(cx))?;
        match self.as_mut().project() {
            OutgoingProj::Opening { .. } => Poll::Ready(Err(corrupted_memory_error())),
            OutgoingProj::Active { tx, .. } => {
                trace!("writing buffer");
                AsyncWrite::poll_write(tx, cx, buf)
            }
        }
    }


    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        ready!(self.as_mut().poll_flush_header(cx))?;
        match self.as_mut().project() {
            OutgoingProj::Opening { .. } => Poll::Ready(Err(corrupted_memory_error())),
            OutgoingProj::Active { tx, .. } => {
                trace!("flushing stream");
                tx.poll_flush(cx)
            }
        }
    }


    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        ready!(self.as_mut().poll_flush_header(cx))?;
        match self.as_mut().project() {
            OutgoingProj::Opening { .. } => Poll::Ready(Err(corrupted_memory_error())),
            OutgoingProj::Active { tx, .. } => {
                trace!("shutting down stream");
                tx.poll_shutdown(cx)
            }
        }
    }
}

async fn demux_connection(
    index: Arc<std::sync::Mutex<IndexTree>>,
    conn: Connection,
) -> std::io::Result<()> {
    loop {
        // TODO: Figure out how to tie the lifetime of this Tokio task to all streams and explicitly close
        // connection once no streams are used
        trace!("accepting async stream");
        let mut rx = match conn.accept_uni().await {
            Ok(rx) => rx,
            Err(err) => match err {
                ConnectionError::ApplicationClosed(ref e) => {
                    if e.error_code != VarInt::default() {
                        return Err(err.into());
                    }
                    return Ok(());
                }
                ConnectionError::LocallyClosed => return Ok(()),
                ConnectionError::VersionMismatch
                | ConnectionError::TransportError(_)
                | ConnectionError::ConnectionClosed(_)
                | ConnectionError::Reset
                | ConnectionError::TimedOut
                | ConnectionError::CidsExhausted => return Err(err.into()),
            },
        };
        // TODO: Use the `CoreVecDecoder`
        trace!("reading path length");
        let n = rx.read_u32_leb128().await?;
        let n = n
            .try_into()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        trace!(n, "read path length");
        let mut path = Vec::with_capacity(n);
        for i in 0..n {
            trace!(i, "reading path element");
            let p = rx.read_u32_leb128().await?;
            let p = p
                .try_into()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            path.push(p);
        }
        trace!("locking path index tree");
        let mut lock = index
            .lock()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;
        let tx = lock.take_tx(&path).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("`{path:?}` subscription not found"),
            )
        })?;
        trace!(?path, "sending stream to receiver");
        tx.send(rx).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stream receiver closed")
        })?;
    }
}

impl wrpc_transport::Invoke for Client {
    type Context = ();
    type Outgoing = Outgoing;
    type Incoming = Incoming;


    async fn invoke<P>(
        &self,
        cx: Self::Context,
        instance: &str,
        func: &str,
        params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> anyhow::Result<(Self::Outgoing, Self::Incoming)>
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
    {
        let san = san(instance, func);
        trace!(?san, "establishing connection");
        let conn = self
            .endpoint
            .connect(self.addr, &san)
            .context("failed to connect to endpoint")?;
        let conn = conn.await.context("failed to establish connection")?;

        trace!("opening parameter stream");
        let (mut param_tx, ret_rx) = conn
            .open_bi()
            .await
            .context("failed to open parameter stream")?;
        let index = Arc::new(std::sync::Mutex::new(paths.as_ref().iter().collect()));
        let io = JoinSet::new();
        // TODO: Use `io`
        tokio::spawn(demux_connection(Arc::clone(&index), conn.clone()).in_current_span());
        let io = Arc::new(io);
        trace!("writing parameters");
        param_tx
            .write_all_chunks(&mut [Bytes::from_static(&[PROTOCOL]), params])
            .await
            .context("failed to write parameters")?;
        Ok((
            Outgoing::Active {
                header: Bytes::default(),
                path: Arc::from([]),
                conn: conn.clone(),
                tx: param_tx,
                io: Arc::clone(&io),
            },
            Incoming::Active {
                index: Arc::clone(&index),
                path: Arc::from([]),
                rx: ret_rx,
                io,
            },
        ))
    }
}


async fn serve_connection(
    conn: Connection,
    paths: &[impl AsRef<[Option<usize>]>],
) -> anyhow::Result<(Outgoing, Incoming)> {
    trace!("accepting parameter stream");
    let (ret_tx, mut param_rx) = conn
        .accept_bi()
        .await
        .context("failed to accept parameter stream")?;
    trace!("reading parameter stream header");
    let x = param_rx
        .read_u8()
        .await
        .context("failed to read parameter stream header")?;
    ensure!(x == PROTOCOL);
    let index = Arc::new(std::sync::Mutex::new(paths.iter().collect()));
    let io = JoinSet::new();
    // TODO: Use `io`
    tokio::spawn(demux_connection(Arc::clone(&index), conn.clone()).in_current_span());
    let io = Arc::new(io);
    Ok((
        Outgoing::Active {
            header: Bytes::default(),
            path: Arc::from([]),
            conn,
            tx: ret_tx,
            io: Arc::clone(&io),
        },
        Incoming::Active {
            index,
            path: Arc::from([]),
            rx: param_rx,
            io,
        },
    ))
}

impl wrpc_transport::Serve for Server {
    type Context = ();
    type Outgoing = Outgoing;
    type Incoming = Incoming;


    async fn serve(
        &self,
        instance: &str,
        func: &str,
        paths: impl Into<Arc<[Box<[Option<usize>]>]>> + Send,
    ) -> anyhow::Result<
        impl Stream<Item = anyhow::Result<(Self::Context, Self::Outgoing, Self::Incoming)>> + 'static,
    > {
        let san = san(instance, func);
        let (tx, rx) = mpsc::channel(1024);
        let mut handlers = self.0.lock().await;
        match handlers.entry(san) {
            hash_map::Entry::Occupied(_) => {
                bail!("handler for `{func}` from `{instance}` already exists")
            }
            hash_map::Entry::Vacant(entry) => {
                entry.insert(tx);
            }
        }
        let paths = paths.into();
        let span = Span::current();
        Ok(ReceiverStream::new(rx).then(move |conn| {
            let paths = Arc::clone(&paths);
            async move {
                let (tx, rx) = serve_connection(conn, &paths).await?;
                Ok(((), tx, rx))
            }

        }))
    }
}
