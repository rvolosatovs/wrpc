//! wRPC NATS.io transport

#![allow(clippy::type_complexity)]

use core::future::Future;
use core::iter::zip;
use core::ops::{Deref, DerefMut};
use core::pin::{pin, Pin};
use core::sync::atomic::AtomicUsize;
use core::task::{ready, Context, Poll};
use core::{mem, str};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, ensure, Context as _};
use bytes::{Buf as _, BufMut as _, Bytes, BytesMut};
use futures::sink::SinkExt as _;
use futures::{Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::select;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::PollSender;
use tracing::{debug, error, instrument, trace, warn};
use wrpc_transport::Index as _;

pub const PROTOCOL: &str = "wrpc.0.0.1";

fn spawn_async(fut: impl Future<Output = ()> + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn(fut);
        }
        Err(_) => match tokio::runtime::Runtime::new() {
            Ok(rt) => {
                rt.spawn(fut);
            }
            Err(err) => error!(?err, "failed to create a new Tokio runtime"),
        },
    }
}

fn new_inbox(inbox: &[u8]) -> Bytes {
    let id = nuid::next();
    let mut s = BytesMut::with_capacity(inbox.len().saturating_add(id.len()));
    s.extend_from_slice(inbox);
    s.extend_from_slice(id.as_bytes());
    s.freeze()
}

#[must_use]
#[inline]
pub fn param_subject(prefix: &[u8]) -> Bytes {
    let mut s = BytesMut::with_capacity(prefix.len().saturating_add(".params".len()));
    s.extend_from_slice(prefix);
    s.extend_from_slice(b".params");
    s.freeze()
}

#[must_use]
#[inline]
pub fn result_subject(prefix: &[u8]) -> Bytes {
    let mut s = BytesMut::with_capacity(prefix.len().saturating_add(".results".len()));
    s.extend_from_slice(prefix);
    s.extend_from_slice(b".results");
    s.freeze()
}

#[must_use]
#[inline]
pub fn index_path(prefix: &[u8], path: &[usize]) -> Bytes {
    let mut s = BytesMut::with_capacity(prefix.len().saturating_add(path.len().saturating_mul(2)));
    if !prefix.is_empty() {
        s.extend_from_slice(prefix);
    }
    for p in path {
        if !s.is_empty() {
            s.put_u8(b'.');
        }
        s.extend_from_slice(p.to_string().as_bytes());
    }
    s.freeze()
}

#[must_use]
#[inline]
pub fn subscribe_path(prefix: &[u8], path: &[Option<usize>]) -> Bytes {
    let mut s = BytesMut::with_capacity(prefix.len().saturating_add(path.len().saturating_mul(2)));
    if !prefix.is_empty() {
        s.extend_from_slice(prefix);
    }
    for p in path {
        if !s.is_empty() {
            s.put_u8(b'.');
        }
        if let Some(p) = p {
            s.extend_from_slice(p.to_string().as_bytes());
        } else {
            s.put_u8(b'*');
        }
    }
    s.freeze()
}

#[must_use]
#[inline]
pub fn invocation_subject(prefix: &[u8], instance: &str, func: &str) -> Bytes {
    let mut s = BytesMut::with_capacity(
        3_usize
            .saturating_add(prefix.len())
            .saturating_add(PROTOCOL.len())
            .saturating_add(instance.len())
            .saturating_add(func.len()),
    );
    if !prefix.is_empty() {
        s.extend_from_slice(prefix);
        s.put_u8(b'.');
    }
    s.extend_from_slice(PROTOCOL.as_bytes());
    s.put_u8(b'.');
    if !instance.is_empty() {
        s.extend_from_slice(instance.as_bytes());
        s.put_u8(b'.');
    }
    s.extend_from_slice(func.as_bytes());
    s.freeze()
}

fn corrupted_memory_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, "corrupted memory state")
}

/// Transport subscriber
pub struct Subscriber {
    rx: ReceiverStream<Message>,
    subject: Bytes,
    commands: mpsc::Sender<Command>,
    tasks: Arc<JoinSet<()>>,
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        let commands = self.commands.clone();
        let subject = mem::take(&mut self.subject);
        let tasks = Arc::clone(&self.tasks);
        spawn_async(async move {
            trace!(?subject, "shutting down subscriber");
            if let Err(err) = commands.send(Command::Unsubscribe(subject)).await {
                warn!(?err, "failed to shutdown subscriber");
            }
            drop(tasks);
        });
    }
}

impl Deref for Subscriber {
    type Target = ReceiverStream<Message>;

    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl DerefMut for Subscriber {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.rx
    }
}

enum Command {
    Subscribe(Bytes, mpsc::Sender<Message>),
    Unsubscribe(Bytes),
    Batch(Box<[Command]>),
}

/// Subset of [`ants::Message`](ants::Message) used by this crate
pub struct Message {
    reply: Bytes,
    payload: Bytes,
}

#[derive(Clone, Debug)]
pub struct Client {
    nats: ants::Client,
    prefix: Bytes,
    inbox: Bytes,
    queue_group: Bytes,
    commands: mpsc::Sender<Command>,
    sid: Arc<AtomicUsize>,
    tasks: Arc<JoinSet<()>>,
}

impl Client {
    fn next_sid(&self) -> Bytes {
        Bytes::from(
            self.sid
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
                .to_string(),
        )
    }
}

#[derive(Default)]
pub struct ClientBuilder {
    prefix: Bytes,
    queue_group: Bytes,
}

impl ClientBuilder {
    pub async fn build(self, nats: ants::Client) -> anyhow::Result<Client> {
        let id = nuid::next();
        let mut subject = BytesMut::with_capacity(9_usize.saturating_add(id.len()));
        subject.extend_from_slice(b"_INBOX.");
        subject.extend_from_slice(id.as_bytes());
        subject.put_u8(b'.');
        let inbox = subject.clone().freeze();

        subject.put_u8(b'>');
        let (sub_tx, mut sub_rx) = mpsc::channel(8196);
        nats.subscribe(subject, Bytes::default(), "0", sub_tx)
            .await
            .context("failed to subscribe on an inbox subject")?;

        let mut tasks = JoinSet::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8192);
        tasks.spawn({
            async move {
                fn handle_command(subs: &mut HashMap<Bytes, mpsc::Sender<Message>>, cmd: Command) {
                    match cmd {
                        Command::Subscribe(s, tx) => {
                            subs.insert(s, tx);
                        }
                        Command::Unsubscribe(s) => {
                            subs.remove(&s);
                        }
                        Command::Batch(cmds) => {
                            for cmd in cmds {
                                handle_command(subs, cmd);
                            }
                        }
                    }
                }
                async fn handle_message(
                    subs: &mut HashMap<Bytes, mpsc::Sender<Message>>,
                    ants::Message {
                        subject,
                        reply,
                        payload,
                        ..
                    }: ants::Message,
                ) {
                    let Some(sub) = subs.get_mut(&subject) else {
                        debug!(?subject, "drop message with no subscriber");
                        return;
                    };
                    let Ok(sub) = sub.reserve().await else {
                        debug!(?subject, "drop message with closed subscriber");
                        subs.remove(&subject);
                        return;
                    };
                    sub.send(Message { reply, payload });
                }

                let mut subs = HashMap::new();
                loop {
                    select! {
                        Some(msg) = sub_rx.recv() => handle_message(&mut subs, msg).await,
                        Some(cmd) = cmd_rx.recv() => handle_command(&mut subs, cmd),
                    }
                }
            }
        });
        Ok(Client {
            nats,
            prefix: self.prefix,
            inbox,
            queue_group: self.queue_group,
            commands: cmd_tx,
            sid: Arc::new(AtomicUsize::new(1)),
            tasks: Arc::new(tasks),
        })
    }
}

impl Client {
    pub async fn new(nats: ants::Client) -> anyhow::Result<Self> {
        ClientBuilder::default().build(nats).await
    }
}

pub struct ByteSubscription(Subscriber);

impl Stream for ByteSubscription {
    type Item = std::io::Result<Bytes>;

    #[instrument(level = "trace", skip_all)]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.0.poll_next_unpin(cx) {
            Poll::Ready(Some(Message { payload, .. })) => Poll::Ready(Some(Ok(payload))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Default)]
enum IndexTrie {
    #[default]
    Empty,
    Leaf(Subscriber),
    IndexNode {
        subscriber: Option<Subscriber>,
        nested: Vec<Option<IndexTrie>>,
    },
    WildcardNode {
        subscriber: Option<Subscriber>,
        nested: Option<Box<IndexTrie>>,
    },
}

impl<'a> From<(&'a [Option<usize>], Subscriber)> for IndexTrie {
    fn from((path, sub): (&'a [Option<usize>], Subscriber)) -> Self {
        match path {
            [] => Self::Leaf(sub),
            [None, path @ ..] => Self::WildcardNode {
                subscriber: None,
                nested: Some(Box::new(Self::from((path, sub)))),
            },
            [Some(i), path @ ..] => Self::IndexNode {
                subscriber: None,
                nested: {
                    let n = i.saturating_add(1);
                    let mut nested = Vec::with_capacity(n);
                    nested.resize_with(n, Option::default);
                    nested[*i] = Some(Self::from((path, sub)));
                    nested
                },
            },
        }
    }
}

impl<P: AsRef<[Option<usize>]>> FromIterator<(P, Subscriber)> for IndexTrie {
    fn from_iter<T: IntoIterator<Item = (P, Subscriber)>>(iter: T) -> Self {
        let mut root = Self::Empty;
        for (path, sub) in iter {
            if !root.insert(path.as_ref(), sub) {
                return Self::Empty;
            }
        }
        root
    }
}

impl IndexTrie {
    #[inline]
    fn is_empty(&self) -> bool {
        matches!(self, IndexTrie::Empty)
    }

    #[instrument(level = "trace", skip_all)]
    fn take(&mut self, path: &[usize]) -> Option<Subscriber> {
        let Some((i, path)) = path.split_first() else {
            return match mem::take(self) {
                // TODO: Demux the subscription
                //IndexTrie::WildcardNode { subscriber, nested } => {
                //    if let Some(nested) = nested {
                //        *self = IndexTrie::WildcardNode {
                //            subscriber: None,
                //            nested: Some(nested),
                //        }
                //    }
                //    subscriber
                //}
                IndexTrie::Empty | IndexTrie::WildcardNode { .. } => None,
                IndexTrie::Leaf(subscriber) => Some(subscriber),
                IndexTrie::IndexNode { subscriber, nested } => {
                    if !nested.is_empty() {
                        *self = IndexTrie::IndexNode {
                            subscriber: None,
                            nested,
                        }
                    }
                    subscriber
                }
            };
        };
        match self {
            // TODO: Demux the subscription
            //Self::WildcardNode { ref mut nested, .. } => {
            //    nested.as_mut().and_then(|nested| nested.take(path))
            //}
            Self::Empty | Self::Leaf(..) | Self::WildcardNode { .. } => None,
            Self::IndexNode { ref mut nested, .. } => nested
                .get_mut(*i)
                .and_then(|nested| nested.as_mut().and_then(|nested| nested.take(path))),
        }
    }

    /// Inserts `sub` under a `path` - returns `false` if it failed and `true` if it succeeded.
    /// Tree state after `false` is returned in undefined
    #[instrument(level = "trace", skip_all)]
    fn insert(&mut self, path: &[Option<usize>], sub: Subscriber) -> bool {
        match self {
            Self::Empty => {
                *self = Self::from((path, sub));
                true
            }
            Self::Leaf(..) => {
                let Some((i, path)) = path.split_first() else {
                    return false;
                };
                let Self::Leaf(subscriber) = mem::take(self) else {
                    return false;
                };
                if let Some(i) = i {
                    let n = i.saturating_add(1);
                    let mut nested = Vec::with_capacity(n);
                    nested.resize_with(n, Option::default);
                    nested[*i] = Some(Self::from((path, sub)));
                    *self = Self::IndexNode {
                        subscriber: Some(subscriber),
                        nested,
                    };
                } else {
                    *self = Self::WildcardNode {
                        subscriber: Some(subscriber),
                        nested: Some(Box::new(Self::from((path, sub)))),
                    };
                }
                true
            }
            Self::WildcardNode {
                ref mut subscriber,
                ref mut nested,
            } => match (&subscriber, path) {
                (None, []) => {
                    *subscriber = Some(sub);
                    true
                }
                (_, [None, path @ ..]) => {
                    if let Some(nested) = nested {
                        nested.insert(path, sub)
                    } else {
                        *nested = Some(Box::new(Self::from((path, sub))));
                        true
                    }
                }
                _ => false,
            },
            Self::IndexNode {
                ref mut subscriber,
                ref mut nested,
            } => match (&subscriber, path) {
                (None, []) => {
                    *subscriber = Some(sub);
                    true
                }
                (_, [Some(i), path @ ..]) => {
                    let cap = i.saturating_add(1);
                    if nested.len() < cap {
                        nested.resize_with(cap, Option::default);
                    }
                    let nested = &mut nested[*i];
                    if let Some(nested) = nested {
                        nested.insert(path, sub)
                    } else {
                        *nested = Some(Self::from((path, sub)));
                        true
                    }
                }
                _ => false,
            },
        }
    }
}

pub struct Reader {
    buffer: Bytes,
    incoming: Option<Subscriber>,
    nested: Arc<std::sync::Mutex<IndexTrie>>,
    path: Box<[usize]>,
}

impl wrpc_transport::Index<Self> for Reader {
    #[instrument(level = "trace", skip(self))]
    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        ensure!(!path.is_empty());
        trace!("locking index tree");
        let mut nested = self
            .nested
            .lock()
            .map_err(|err| anyhow!(err.to_string()).context("failed to lock map"))?;
        trace!("taking index subscription");
        let mut p = self.path.to_vec();
        p.extend_from_slice(path);
        let incoming = nested.take(&p);
        Ok(Self {
            buffer: Bytes::default(),
            incoming,
            nested: Arc::clone(&self.nested),
            path: p.into_boxed_slice(),
        })
    }
}

impl AsyncRead for Reader {
    #[instrument(level = "trace", skip_all, ret)]
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let cap = buf.remaining();
        if cap == 0 {
            trace!("attempt to read empty buffer");
            return Poll::Ready(Ok(()));
        }

        if !self.buffer.is_empty() {
            if self.buffer.len() > cap {
                trace!(cap, len = self.buffer.len(), "reading part of buffer");
                buf.put_slice(&self.buffer.split_to(cap));
            } else {
                trace!(cap, len = self.buffer.len(), "reading full buffer");
                buf.put_slice(&mem::take(&mut self.buffer));
            }
            return Poll::Ready(Ok(()));
        }
        let Some(incoming) = self.incoming.as_mut() else {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("subscription not found for path {:?}", self.path),
            )));
        };
        trace!("polling for next message");
        match incoming.poll_next_unpin(cx) {
            Poll::Ready(Some(Message { mut payload, .. })) => {
                trace!(?payload, "received message");
                if payload.is_empty() {
                    trace!("received stream shutdown message");
                    return Poll::Ready(Ok(()));
                }
                if payload.len() > cap {
                    trace!(len = payload.len(), cap, "partially reading the message");
                    buf.put_slice(&payload.split_to(cap));
                    self.buffer = payload;
                } else {
                    trace!(len = payload.len(), cap, "filling the buffer with payload");
                    buf.put_slice(&payload);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => {
                trace!("subscription finished");
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone)]
pub struct SubjectWriter {
    nats: PollSender<ants::Command>,
    info: watch::Receiver<Arc<ants::protocol::InfoOptions>>,
    tx: Bytes,
    shutdown: bool,
    tasks: Arc<JoinSet<()>>,
}

impl SubjectWriter {
    fn new(
        nats: PollSender<ants::Command>,
        info: watch::Receiver<Arc<ants::protocol::InfoOptions>>,
        tx: Bytes,
        tasks: Arc<JoinSet<()>>,
    ) -> Self {
        Self {
            nats,
            info,
            tx,
            shutdown: false,
            tasks,
        }
    }
}

impl wrpc_transport::Index<Self> for SubjectWriter {
    #[instrument(level = "trace", skip(self))]
    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        ensure!(!path.is_empty());
        let tx = index_path(&self.tx, path);
        Ok(Self {
            nats: self.nats.clone(),
            info: self.info.clone(),
            tx,
            shutdown: false,
            tasks: Arc::clone(&self.tasks),
        })
    }
}

impl AsyncWrite for SubjectWriter {
    #[instrument(level = "trace", skip_all, ret, fields(subject = ?self.tx, buf = format!("{buf:02x?}")))]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        trace!("polling for readiness");
        match self.nats.poll_ready_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(..)) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Poll::Ready(Ok(())) => {}
        }
        let max_payload = self.info.borrow().max_payload;
        if max_payload == 0 {
            return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
        }
        let total = buf.len();
        let cmd = if total < max_payload {
            ants::Command::Publish {
                subject: self.tx.clone(),
                payload: Bytes::copy_from_slice(buf),
                reply: Bytes::default(),
                headers: Bytes::default(),
            }
        } else {
            let mut cap = max_payload.saturating_div(total);
            let rem = max_payload % total;
            if rem > 0 {
                cap = cap.saturating_add(1);
            }
            let mut cmds = Vec::with_capacity(cap);
            while buf.len() > max_payload {
                (buf, _) = buf.split_at(max_payload);
                cmds.push(ants::Command::Publish {
                    subject: self.tx.clone(),
                    payload: Bytes::copy_from_slice(buf),
                    reply: Bytes::default(),
                    headers: Bytes::default(),
                })
            }
            if !buf.is_empty() {
                cmds.push(ants::Command::Publish {
                    subject: self.tx.clone(),
                    payload: Bytes::copy_from_slice(buf),
                    reply: Bytes::default(),
                    headers: Bytes::default(),
                })
            }
            ants::Command::Batch(cmds.into_boxed_slice())
        };
        trace!("starting send");
        match self.nats.start_send_unpin(cmd) {
            Ok(()) => Poll::Ready(Ok(total)),
            Err(..) => Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))),
        }
    }

    #[instrument(level = "trace", skip_all, ret, fields(subject = ?self.tx))]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        trace!("polling for readiness");
        match self.nats.poll_ready_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(..)) => return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            Poll::Ready(Ok(())) => {}
        }
        trace!("flushing");
        match self.nats.start_send_unpin(ants::Command::Flush) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(..) => Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))),
        }
    }

    #[instrument(level = "trace", skip_all, ret, fields(subject = ?self.tx))]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        trace!("writing stream shutdown message");
        ready!(self.as_mut().poll_write(cx, &[]))?;
        self.shutdown = true;
        Poll::Ready(Ok(()))
    }
}

impl Drop for SubjectWriter {
    fn drop(&mut self) {
        if !self.shutdown {
            let mut nats = self.nats.clone();
            let subject = mem::take(&mut self.tx);
            let tasks = Arc::clone(&self.tasks);
            spawn_async(async move {
                trace!("writing stream shutdown message");
                if let Err(_) = nats
                    .send(ants::Command::Publish {
                        subject,
                        reply: Bytes::default(),
                        headers: Bytes::default(),
                        payload: Bytes::default(),
                    })
                    .await
                {
                    warn!("failed to publish stream shutdown message");
                }
                drop(tasks);
            });
        }
    }
}

#[derive(Default)]
pub enum RootParamWriter {
    #[default]
    Corrupted,
    Handshaking {
        nats: ants::Client,
        info: watch::Receiver<Arc<ants::protocol::InfoOptions>>,
        sub: Subscriber,
        indexed: std::sync::Mutex<Vec<(Vec<usize>, oneshot::Sender<SubjectWriter>)>>,
        buffer: Bytes,
        tasks: Arc<JoinSet<()>>,
    },
    Draining {
        tx: SubjectWriter,
        buffer: Bytes,
    },
    Active(SubjectWriter),
}

impl RootParamWriter {
    fn new(
        nats: ants::Client,
        info: watch::Receiver<Arc<ants::protocol::InfoOptions>>,
        sub: Subscriber,
        buffer: Bytes,
        tasks: Arc<JoinSet<()>>,
    ) -> Self {
        Self::Handshaking {
            nats,
            info,
            sub,
            indexed: std::sync::Mutex::default(),
            buffer,
            tasks,
        }
    }
}

impl RootParamWriter {
    #[instrument(level = "trace", skip_all, ret)]
    fn poll_active(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Corrupted => Poll::Ready(Err(corrupted_memory_error())),
            Self::Handshaking { sub, .. } => {
                trace!("polling for handshake response");
                match sub.poll_next_unpin(cx) {
                    Poll::Ready(Some(Message { reply: tx, .. })) => {
                        if tx.is_empty() {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "peer did not specify a reply subject",
                            )));
                        }
                        let Self::Handshaking {
                            nats,
                            info,
                            indexed,
                            buffer,
                            tasks,
                            ..
                        } = mem::take(&mut *self)
                        else {
                            return Poll::Ready(Err(corrupted_memory_error()));
                        };
                        let tx = SubjectWriter::new(
                            PollSender::new(nats.commands().clone()),
                            info,
                            param_subject(&tx),
                            tasks,
                        );
                        let indexed = indexed.into_inner().map_err(|err| {
                            std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
                        })?;
                        for (path, tx_tx) in indexed {
                            let tx = tx.index(&path).map_err(|err| {
                                std::io::Error::new(std::io::ErrorKind::Other, err)
                            })?;
                            tx_tx.send(tx).map_err(|_| {
                                std::io::Error::from(std::io::ErrorKind::BrokenPipe)
                            })?;
                        }
                        trace!("handshake succeeded");
                        if buffer.is_empty() {
                            *self = Self::Active(tx);
                            Poll::Ready(Ok(()))
                        } else {
                            *self = Self::Draining { tx, buffer };
                            self.poll_active(cx)
                        }
                    }
                    Poll::Ready(None) => {
                        *self = Self::Corrupted;
                        Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            Self::Draining { tx, buffer } => {
                let mut tx = pin!(tx);
                while !buffer.is_empty() {
                    trace!(?tx.tx, "draining parameter buffer");
                    match tx.as_mut().poll_write(cx, buffer) {
                        Poll::Ready(Ok(n)) => {
                            buffer.advance(n);
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                let Self::Draining { tx, .. } = mem::take(&mut *self) else {
                    return Poll::Ready(Err(corrupted_memory_error()));
                };
                trace!("parameter buffer draining succeeded");
                *self = Self::Active(tx);
                Poll::Ready(Ok(()))
            }
            Self::Active(..) => Poll::Ready(Ok(())),
        }
    }
}

impl wrpc_transport::Index<IndexedParamWriter> for RootParamWriter {
    #[instrument(level = "trace", skip(self))]
    fn index(&self, path: &[usize]) -> anyhow::Result<IndexedParamWriter> {
        ensure!(!path.is_empty());
        match self {
            Self::Corrupted => Err(anyhow!(corrupted_memory_error())),
            Self::Handshaking { indexed, .. } => {
                let (tx_tx, tx_rx) = oneshot::channel();
                let mut indexed = indexed.lock().map_err(|err| {
                    std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
                })?;
                indexed.push((path.to_vec(), tx_tx));
                Ok(IndexedParamWriter::Handshaking {
                    tx_rx,
                    indexed: std::sync::Mutex::default(),
                })
            }
            Self::Draining { tx, .. } | Self::Active(tx) => {
                tx.index(path).map(IndexedParamWriter::Active)
            }
        }
    }
}

impl AsyncWrite for RootParamWriter {
    #[instrument(level = "trace", skip_all, ret, fields(buf = format!("{buf:02x?}")))]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.as_mut().poll_active(cx)? {
            Poll::Ready(()) => {
                let Self::Active(tx) = &mut *self else {
                    return Poll::Ready(Err(corrupted_memory_error()));
                };
                trace!("writing buffer");
                pin!(tx).poll_write(cx, buf)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    #[instrument(level = "trace", skip_all, ret)]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().poll_active(cx)? {
            Poll::Ready(()) => {
                let Self::Active(tx) = &mut *self else {
                    return Poll::Ready(Err(corrupted_memory_error()));
                };
                trace!("flushing");
                pin!(tx).poll_flush(cx)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    #[instrument(level = "trace", skip_all, ret)]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().poll_active(cx)? {
            Poll::Ready(()) => {
                let Self::Active(tx) = &mut *self else {
                    return Poll::Ready(Err(corrupted_memory_error()));
                };
                trace!("shutting down");
                pin!(tx).poll_shutdown(cx)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Default)]
pub enum IndexedParamWriter {
    #[default]
    Corrupted,
    Handshaking {
        tx_rx: oneshot::Receiver<SubjectWriter>,
        indexed: std::sync::Mutex<Vec<(Vec<usize>, oneshot::Sender<SubjectWriter>)>>,
    },
    Active(SubjectWriter),
}

impl IndexedParamWriter {
    #[instrument(level = "trace", skip_all, ret)]
    fn poll_active(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Corrupted => Poll::Ready(Err(corrupted_memory_error())),
            Self::Handshaking { tx_rx, .. } => {
                trace!("polling for handshake");
                match pin!(tx_rx).poll(cx) {
                    Poll::Ready(Ok(tx)) => {
                        let Self::Handshaking { indexed, .. } = mem::take(&mut *self) else {
                            return Poll::Ready(Err(corrupted_memory_error()));
                        };
                        let indexed = indexed.into_inner().map_err(|err| {
                            std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
                        })?;
                        for (path, tx_tx) in indexed {
                            let tx = tx.index(&path).map_err(|err| {
                                std::io::Error::new(std::io::ErrorKind::Other, err)
                            })?;
                            tx_tx.send(tx).map_err(|_| {
                                std::io::Error::from(std::io::ErrorKind::BrokenPipe)
                            })?;
                        }
                        *self = Self::Active(tx);
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Err(..)) => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
                    Poll::Pending => Poll::Pending,
                }
            }
            Self::Active(..) => Poll::Ready(Ok(())),
        }
    }
}

impl wrpc_transport::Index<Self> for IndexedParamWriter {
    #[instrument(level = "trace", skip_all)]
    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        ensure!(!path.is_empty());
        match self {
            Self::Corrupted => Err(anyhow!(corrupted_memory_error())),
            Self::Handshaking { indexed, .. } => {
                let (tx_tx, tx_rx) = oneshot::channel();
                let mut indexed = indexed.lock().map_err(|err| {
                    std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
                })?;
                indexed.push((path.to_vec(), tx_tx));
                Ok(Self::Handshaking {
                    tx_rx,
                    indexed: std::sync::Mutex::default(),
                })
            }
            Self::Active(tx) => tx.index(path).map(Self::Active),
        }
    }
}

impl AsyncWrite for IndexedParamWriter {
    #[instrument(level = "trace", skip_all, ret, fields(buf = format!("{buf:02x?}")))]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.as_mut().poll_active(cx)? {
            Poll::Ready(()) => {
                let Self::Active(tx) = &mut *self else {
                    return Poll::Ready(Err(corrupted_memory_error()));
                };
                trace!("writing buffer");
                pin!(tx).poll_write(cx, buf)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    #[instrument(level = "trace", skip_all, ret)]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().poll_active(cx)? {
            Poll::Ready(()) => {
                let Self::Active(tx) = &mut *self else {
                    return Poll::Ready(Err(corrupted_memory_error()));
                };
                trace!("flushing");
                pin!(tx).poll_flush(cx)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    #[instrument(level = "trace", skip_all, ret)]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().poll_active(cx)? {
            Poll::Ready(()) => {
                let Self::Active(tx) = &mut *self else {
                    return Poll::Ready(Err(corrupted_memory_error()));
                };
                trace!("shutting down");
                pin!(tx).poll_shutdown(cx)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub enum ParamWriter {
    Root(RootParamWriter),
    Nested(IndexedParamWriter),
}

impl wrpc_transport::Index<Self> for ParamWriter {
    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        ensure!(!path.is_empty());
        match self {
            ParamWriter::Root(w) => w.index(path),
            ParamWriter::Nested(w) => w.index(path),
        }
        .map(Self::Nested)
    }
}

impl AsyncWrite for ParamWriter {
    #[instrument(level = "trace", skip_all, ret, fields(buf = format!("{buf:02x?}")))]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            ParamWriter::Root(w) => pin!(w).poll_write(cx, buf),
            ParamWriter::Nested(w) => pin!(w).poll_write(cx, buf),
        }
    }

    #[instrument(level = "trace", skip_all, ret)]
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            ParamWriter::Root(w) => pin!(w).poll_flush(cx),
            ParamWriter::Nested(w) => pin!(w).poll_flush(cx),
        }
    }

    #[instrument(level = "trace", skip_all, ret)]
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            ParamWriter::Root(w) => pin!(w).poll_shutdown(cx),
            ParamWriter::Nested(w) => pin!(w).poll_shutdown(cx),
        }
    }
}

impl wrpc_transport::Invoke for Client {
    type Context = Bytes;
    type Outgoing = ParamWriter;
    type Incoming = Reader;

    #[instrument(level = "trace", skip(self, paths, params), fields(params = format!("{params:02x?}")))]
    async fn invoke<P: AsRef<[Option<usize>]> + Send + Sync>(
        &self,
        headers: Self::Context,
        instance: &str,
        func: &str,
        mut params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> anyhow::Result<(Self::Outgoing, Self::Incoming)> {
        let paths = paths.as_ref();
        let mut cmds = Vec::with_capacity(paths.len().saturating_add(2));

        let rx = new_inbox(&self.inbox);
        let (handshake_tx, handshake_rx) = mpsc::channel(1);
        cmds.push(Command::Subscribe(rx.clone(), handshake_tx));

        let result = result_subject(&rx);
        let (result_tx, result_rx) = mpsc::channel(16);
        cmds.push(Command::Subscribe(result.clone(), result_tx));

        let nested = paths.iter().map(|path| {
            let (tx, rx) = mpsc::channel(16);
            let subject = subscribe_path(&result, path.as_ref());
            cmds.push(Command::Subscribe(subject.clone(), tx));
            Subscriber {
                rx: ReceiverStream::new(rx),
                commands: self.commands.clone(),
                subject,
                tasks: Arc::clone(&self.tasks),
            }
        });
        let nested: IndexTrie = zip(paths.iter(), nested).collect();
        ensure!(
            paths.is_empty() == nested.is_empty(),
            "failed to construct subscription tree"
        );

        self.commands
            .send(Command::Batch(cmds.into_boxed_slice()))
            .await
            .context("failed to subscribe")?;

        let info = self
            .nats
            .server_info()
            .await
            .context("failed to get server info")?;
        let mut max_payload = info.borrow().max_payload;
        let param_tx = invocation_subject(&self.prefix, instance, func);
        if !headers.is_empty() {
            max_payload = max_payload.saturating_sub(headers.len());
        }
        trace!("publishing handshake");
        self.nats
            .publish(
                param_tx,
                rx.clone(),
                headers,
                params.split_to(max_payload.min(params.len())),
            )
            .await
            .context("failed to publish handshake")?;
        Ok((
            ParamWriter::Root(RootParamWriter::new(
                self.nats.clone(),
                info,
                Subscriber {
                    rx: ReceiverStream::new(handshake_rx),
                    commands: self.commands.clone(),
                    subject: rx,
                    tasks: Arc::clone(&self.tasks),
                },
                params,
                Arc::clone(&self.tasks),
            )),
            Reader {
                buffer: Bytes::default(),
                incoming: Some(Subscriber {
                    rx: ReceiverStream::new(result_rx),
                    commands: self.commands.clone(),
                    subject: result,
                    tasks: Arc::clone(&self.tasks),
                }),
                nested: Arc::new(std::sync::Mutex::new(nested)),
                path: Box::default(),
            },
        ))
    }
}

async fn handle_message(
    nats: &ants::Client,
    rx: Bytes,
    commands: mpsc::Sender<Command>,
    ants::Message {
        reply: tx,
        payload,
        headers,
        ..
    }: ants::Message,
    paths: &[Box<[Option<usize>]>],
    tasks: Arc<JoinSet<()>>,
) -> anyhow::Result<(Bytes, SubjectWriter, Reader)> {
    if tx.is_empty() {
        bail!("peer did not specify a reply subject")
    }

    let mut cmds = Vec::with_capacity(paths.len().saturating_add(1));

    let param = Bytes::from(param_subject(&rx));
    let (param_tx, param_rx) = mpsc::channel(16);
    cmds.push(Command::Subscribe(param.clone(), param_tx));

    let nested = paths.iter().map(|path| {
        let (tx, rx) = mpsc::channel(16);
        let subject = Bytes::from(subscribe_path(&param, path.as_ref()));
        cmds.push(Command::Subscribe(subject.clone(), tx));
        Subscriber {
            rx: ReceiverStream::new(rx),
            commands: commands.clone(),
            subject,
            tasks: Arc::clone(&tasks),
        }
    });
    let nested: IndexTrie = zip(paths.iter(), nested).collect();
    ensure!(
        paths.is_empty() == nested.is_empty(),
        "failed to construct subscription tree"
    );

    commands
        .send(Command::Batch(cmds.into_boxed_slice()))
        .await
        .context("failed to subscribe")?;

    trace!("publishing handshake response");
    nats.publish(tx.clone(), rx, Bytes::default(), Bytes::default())
        .await
        .context("failed to publish handshake accept")?;
    let info = nats
        .server_info()
        .await
        .context("failed to get server info")?;
    Ok((
        headers,
        SubjectWriter::new(
            PollSender::new(nats.commands().clone()),
            info,
            result_subject(&tx),
            Arc::clone(&tasks),
        ),
        Reader {
            buffer: payload,
            incoming: Some(Subscriber {
                rx: ReceiverStream::new(param_rx),
                commands,
                subject: param,
                tasks,
            }),
            nested: Arc::new(std::sync::Mutex::new(nested)),
            path: Box::default(),
        },
    ))
}

impl wrpc_transport::Serve for Client {
    type Context = Bytes;
    type Outgoing = SubjectWriter;
    type Incoming = Reader;

    #[instrument(level = "trace", skip(self, paths))]
    async fn serve(
        &self,
        instance: &str,
        func: &str,
        paths: impl Into<Arc<[Box<[Option<usize>]>]>> + Send,
    ) -> anyhow::Result<
        impl Stream<Item = anyhow::Result<(Self::Context, Self::Outgoing, Self::Incoming)>> + 'static,
    > {
        let subject = invocation_subject(&self.prefix, instance, func);
        debug!(?subject, "subscribing on invocation subject");
        let (sub_tx, sub_rx) = mpsc::channel(256);
        self.nats
            .subscribe(subject, self.queue_group.clone(), self.next_sid(), sub_tx)
            .await?;
        let nats = self.nats.clone();
        let paths = paths.into();
        let commands = self.commands.clone();
        let inbox = self.inbox.clone();
        let tasks = Arc::clone(&self.tasks);
        Ok(ReceiverStream::new(sub_rx).then(move |msg| {
            let tasks = Arc::clone(&tasks);
            let nats = nats.clone();
            let paths = Arc::clone(&paths);
            let commands = commands.clone();
            let rx = new_inbox(&inbox);
            async move { handle_message(&nats, rx, commands, msg, &paths, tasks).await }
        }))
    }
}
