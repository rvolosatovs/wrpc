#![allow(clippy::type_complexity)]

use core::future::Future;
use core::iter::zip;
use core::pin::{pin, Pin};
use core::task::{ready, Context, Poll};
use core::{mem, str};

use std::sync::Arc;

use anyhow::{anyhow, ensure, Context as _};
use async_nats::{HeaderMap, Message, PublishMessage, ServerInfo, StatusCode, Subject, Subscriber};
use bytes::{Buf as _, Bytes};
use futures::future::try_join_all;
use futures::sink::SinkExt as _;
use futures::{Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::oneshot;
use tokio::try_join;
use tracing::{debug, instrument, trace, warn};

use wrpc_transport::Index as _;

pub const PROTOCOL: &str = "wrpc.0.0.1";

#[must_use]
#[inline]
pub fn param_subject(prefix: &str) -> String {
    format!("{prefix}.params")
}

#[must_use]
#[inline]
pub fn result_subject(prefix: &str) -> String {
    format!("{prefix}.results")
}

#[must_use]
#[inline]
pub fn index_path(prefix: &str, path: &[usize]) -> String {
    let mut s = String::with_capacity(prefix.len() + path.len() * 2);
    if !prefix.is_empty() {
        s.push_str(prefix);
    }
    for p in path {
        if !s.is_empty() {
            s.push('.');
        }
        s.push_str(&p.to_string());
    }
    s
}

#[must_use]
#[inline]
pub fn subscribe_path(prefix: &str, path: &[Option<usize>]) -> String {
    let mut s = String::with_capacity(prefix.len() + path.len() * 2);
    if !prefix.is_empty() {
        s.push_str(prefix);
    }
    for p in path {
        if !s.is_empty() {
            s.push('.');
        }
        if let Some(p) = p {
            s.push_str(&p.to_string());
        } else {
            s.push('*');
        }
    }
    s
}

#[must_use]
#[inline]
pub fn invocation_subject(prefix: &str, instance: &str, func: &str) -> String {
    let mut s =
        String::with_capacity(prefix.len() + PROTOCOL.len() + instance.len() + func.len() + 3);
    if !prefix.is_empty() {
        s.push_str(prefix);
        s.push('.');
    }
    s.push_str(PROTOCOL);
    s.push('.');
    if !instance.is_empty() {
        s.push_str(instance);
        s.push('.');
    }
    s.push_str(func);
    s
}

fn corrupted_memory_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, "corrupted memory state")
}

#[derive(Clone, Debug)]
pub struct Client {
    nats: Arc<async_nats::Client>,
    prefix: Arc<str>,
    queue_group: Option<Arc<str>>,
}

impl Client {
    pub fn new(
        nats: impl Into<Arc<async_nats::Client>>,
        prefix: impl Into<Arc<str>>,
        queue_group: Option<Arc<str>>,
    ) -> Self {
        Self {
            nats: nats.into(),
            prefix: prefix.into(),
            queue_group,
        }
    }
}

#[derive(Debug)]
pub struct ByteSubscription(Subscriber);

impl Stream for ByteSubscription {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.0.poll_next_unpin(cx) {
            Poll::Ready(Some(Message { payload, .. })) => Poll::Ready(Some(Ok(payload))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Default)]
enum SubscriberTree {
    #[default]
    Empty,
    Leaf(Subscriber),
    IndexNode {
        subscriber: Option<Subscriber>,
        nested: Vec<Option<SubscriberTree>>,
    },
    WildcardNode {
        subscriber: Option<Subscriber>,
        nested: Option<Box<SubscriberTree>>,
    },
}

impl<'a> From<(&'a [Option<usize>], Subscriber)> for SubscriberTree {
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

impl<P: AsRef<[Option<usize>]>> FromIterator<(P, Subscriber)> for SubscriberTree {
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

impl SubscriberTree {
    #[inline]
    fn is_empty(&self) -> bool {
        matches!(self, SubscriberTree::Empty)
    }

    fn take(&mut self, path: &[usize]) -> Option<Subscriber> {
        let Some((i, path)) = path.split_first() else {
            return match mem::take(self) {
                SubscriberTree::Empty => None,
                SubscriberTree::Leaf(subscriber) => Some(subscriber),
                SubscriberTree::IndexNode { subscriber, nested } => {
                    if !nested.is_empty() {
                        *self = SubscriberTree::IndexNode {
                            subscriber: None,
                            nested,
                        }
                    }
                    subscriber
                }
                SubscriberTree::WildcardNode { .. } => None,
                // TODO: Demux the subscription
                //SubscriberTree::WildcardNode { subscriber, nested } => {
                //    if let Some(nested) = nested {
                //        *self = SubscriberTree::WildcardNode {
                //            subscriber: None,
                //            nested: Some(nested),
                //        }
                //    }
                //    subscriber
                //}
            };
        };
        match self {
            Self::Empty | Self::Leaf(..) => None,
            Self::IndexNode { ref mut nested, .. } => nested
                .get_mut(*i)
                .and_then(|nested| nested.as_mut().and_then(|nested| nested.take(path))),
            Self::WildcardNode { .. } => None,
            // TODO: Demux the subscription
            //Self::WildcardNode { ref mut nested, .. } => {
            //    nested.as_mut().and_then(|nested| nested.take(path))
            //}
        }
    }

    /// Inserts `sub` under a `path` - returns `false` if it failed and `true` if it succeeded.
    /// Tree state after `false` is returned in undefined

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
    incoming: Subscriber,
    nested: Arc<std::sync::Mutex<SubscriberTree>>,
}

impl wrpc_transport::Index<Self> for Reader {
    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        trace!("locking index tree");
        let mut nested = self
            .nested
            .lock()
            .map_err(|err| anyhow!(err.to_string()).context("failed to lock map"))?;
        trace!("taking index subscription");
        let incoming = nested
            .take(path)
            .with_context(|| format!("unknown subscription for path `{path:?}`"))?;
        Ok(Self {
            buffer: Bytes::default(),
            incoming,
            nested: Arc::clone(&self.nested),
        })
    }
}

impl AsyncRead for Reader {
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
        trace!("polling for next message");
        match self.incoming.poll_next_unpin(cx) {
            Poll::Ready(Some(Message { mut payload, .. })) => {
                trace!(?payload, "received message");
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

#[derive(Clone, Debug)]
pub struct SubjectWriter {
    nats: async_nats::Client,
    tx: Subject,
}

impl SubjectWriter {
    fn new(nats: async_nats::Client, tx: Subject) -> Self {
        Self { nats, tx }
    }
}

impl wrpc_transport::Index<Self> for SubjectWriter {
    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
        let tx = Subject::from(index_path(self.tx.as_str(), path));
        Ok(Self {
            nats: self.nats.clone(),
            tx,
        })
    }
}

impl AsyncWrite for SubjectWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        trace!("polling for readiness");
        match self.nats.poll_ready_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(err)) => {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    err,
                )))
            }
            Poll::Ready(Ok(())) => {}
        }
        let ServerInfo { max_payload, .. } = self.nats.server_info();
        if max_payload == 0 {
            return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
        }
        if buf.len() > max_payload {
            (buf, _) = buf.split_at(max_payload);
        }
        trace!("starting send");
        let subject = self.tx.clone();
        match self.nats.start_send_unpin(PublishMessage {
            subject,
            payload: Bytes::copy_from_slice(buf),
            reply: None,
            headers: None,
        }) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(err) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                err,
            ))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        trace!("flushing");
        self.nats
            .poll_flush_unpin(cx)
            .map_err(|_| std::io::ErrorKind::BrokenPipe.into())
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        trace!("writing empty buffer to shut down stream");
        ready!(self.as_mut().poll_write(cx, &[]))?;
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug, Default)]
pub enum RootParamWriter {
    #[default]
    Corrupted,
    Handshaking {
        tx: SubjectWriter,
        sub: Subscriber,
        indexed: std::sync::Mutex<Vec<(Vec<usize>, oneshot::Sender<SubjectWriter>)>>,
        buffer: Bytes,
    },
    Draining {
        tx: SubjectWriter,
        buffer: Bytes,
    },
    Active(SubjectWriter),
}

impl RootParamWriter {
    fn new(tx: SubjectWriter, sub: Subscriber, buffer: Bytes) -> Self {
        Self::Handshaking {
            tx,
            sub,
            indexed: std::sync::Mutex::default(),
            buffer,
        }
    }
}

impl RootParamWriter {
    fn poll_active(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Corrupted => Poll::Ready(Err(corrupted_memory_error())),
            Self::Handshaking { sub, .. } => {
                trace!("polling for handshake response");
                match sub.poll_next_unpin(cx) {
                    Poll::Ready(Some(Message {
                        status: Some(StatusCode::NO_RESPONDERS),
                        ..
                    })) => Poll::Ready(Err(std::io::ErrorKind::NotConnected.into())),
                    Poll::Ready(Some(Message {
                        status: Some(StatusCode::TIMEOUT),
                        ..
                    })) => Poll::Ready(Err(std::io::ErrorKind::TimedOut.into())),
                    Poll::Ready(Some(Message {
                        status: Some(StatusCode::REQUEST_TERMINATED),
                        ..
                    })) => Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into())),
                    Poll::Ready(Some(Message {
                        status: Some(code),
                        description,
                        ..
                    })) if !code.is_success() => Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        if let Some(description) = description {
                            format!("received a response with code `{code}` ({description})")
                        } else {
                            format!("received a response with code `{code}`")
                        },
                    ))),
                    Poll::Ready(Some(Message {
                        reply: Some(tx), ..
                    })) => {
                        let Self::Handshaking {
                            tx: SubjectWriter { nats, .. },
                            indexed,
                            buffer,
                            ..
                        } = mem::take(&mut *self)
                        else {
                            return Poll::Ready(Err(corrupted_memory_error()));
                        };
                        let tx = SubjectWriter::new(nats, Subject::from(param_subject(&tx)));
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
                    Poll::Ready(Some(..)) => Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "peer did not specify a reply subject",
                    ))),
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
    fn index(&self, path: &[usize]) -> anyhow::Result<IndexedParamWriter> {
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

#[derive(Debug, Default)]
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
    fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
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
        match self {
            ParamWriter::Root(w) => w.index(path),
            ParamWriter::Nested(w) => w.index(path),
        }
        .map(Self::Nested)
    }
}

impl AsyncWrite for ParamWriter {
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

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            ParamWriter::Root(w) => pin!(w).poll_flush(cx),
            ParamWriter::Nested(w) => pin!(w).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            ParamWriter::Root(w) => pin!(w).poll_shutdown(cx),
            ParamWriter::Nested(w) => pin!(w).poll_shutdown(cx),
        }
    }
}

impl wrpc_transport::Invoke for Client {
    type Context = Option<HeaderMap>;
    type Outgoing = ParamWriter;
    type Incoming = Reader;

    async fn invoke<P: AsRef<[Option<usize>]> + Send + Sync>(
        &self,
        cx: Self::Context,
        instance: &str,
        func: &str,
        mut params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> anyhow::Result<(Self::Outgoing, Self::Incoming)> {
        let rx = Subject::from(self.nats.new_inbox());
        let result_rx = Subject::from(result_subject(&rx));
        let paths = paths.as_ref();
        let (result_rx, handshake_rx, nested) = try_join!(
            async {
                self.nats
                    .subscribe(result_rx.clone())
                    .await
                    .context("failed to subscribe on result subject")
            },
            async {
                self.nats
                    .subscribe(rx.clone())
                    .await
                    .context("failed to subscribe on handshake subject")
            },
            try_join_all(paths.iter().map(|path| async {
                self.nats
                    .subscribe(Subject::from(subscribe_path(&result_rx, path.as_ref())))
                    .await
                    .context("failed to subscribe on nested result subject")
            }))
        )?;
        let nested: SubscriberTree = zip(paths.iter(), nested).collect();
        ensure!(
            paths.is_empty() == nested.is_empty(),
            "failed to construct subscription tree"
        );
        let ServerInfo {
            mut max_payload, ..
        } = self.nats.server_info();
        max_payload = max_payload.saturating_sub(rx.len());
        let param_tx = Subject::from(invocation_subject(&self.prefix, instance, func));
        if let Some(headers) = cx {
            // based on https://github.com/nats-io/nats.rs/blob/0942c473ce56163fdd1fbc62762f8164e3afa7bf/async-nats/src/header.rs#L215-L224
            max_payload = max_payload
                .saturating_sub(b"NATS/1.0\r\n".len())
                .saturating_sub(b"\r\n".len());
            for (k, vs) in headers.iter() {
                let k: &[u8] = k.as_ref();
                for v in vs {
                    max_payload = max_payload
                        .saturating_sub(k.len())
                        .saturating_sub(b": ".len())
                        .saturating_sub(v.as_str().len())
                        .saturating_sub(b"\r\n".len());
                }
            }
            trace!("publishing handshake");
            self.nats
                .publish_with_reply_and_headers(
                    param_tx.clone(),
                    rx,
                    headers,
                    params.split_to(max_payload.min(params.len())),
                )
                .await
        } else {
            trace!("publishing handshake");
            self.nats
                .publish_with_reply(
                    param_tx.clone(),
                    rx,
                    params.split_to(max_payload.min(params.len())),
                )
                .await
        }
        .context("failed to send handshake")?;
        Ok((
            ParamWriter::Root(RootParamWriter::new(
                SubjectWriter::new((*self.nats).clone(), param_tx),
                handshake_rx,
                params,
            )),
            Reader {
                buffer: Bytes::default(),
                incoming: result_rx,
                nested: Arc::new(std::sync::Mutex::new(nested)),
            },
        ))
    }
}

impl wrpc_transport::Serve for Client {
    type Context = Option<HeaderMap>;
    type Outgoing = SubjectWriter;
    type Incoming = Reader;

    async fn serve(
        &self,
        instance: &str,
        func: &str,
        paths: impl Into<Arc<[Box<[Option<usize>]>]>> + Send,
    ) -> anyhow::Result<
        impl Stream<Item = anyhow::Result<(Self::Context, Self::Outgoing, Self::Incoming)>> + 'static,
    > {
        let subject = invocation_subject(&self.prefix, instance, func);
        let sub = if let Some(group) = &self.queue_group {
            debug!(subject, ?group, "queue-subscribing on invocation subject");
            self.nats
                .queue_subscribe(subject, group.to_string())
                .await?
        } else {
            debug!(subject, "subscribing on invocation subject");
            self.nats.subscribe(subject).await?
        };
        let paths = paths.into();
        let nats = Arc::clone(&self.nats);
        Ok(sub.then(
            // NOTE: instrumenting this function causes a stack overflow
            move |Message {
                      reply: tx,
                      payload,
                      headers,
                      ..
                  }: Message| {
                {
                    let nats = Arc::clone(&nats);
                    let paths = Arc::clone(&paths);
                    async move {
                        let tx = tx.context("peer did not specify a reply subject")?;
                        let rx = nats.new_inbox();
                        let param_rx = Subject::from(param_subject(&rx));
                        let (param_rx, nested) = try_join!(
                            async {
                                trace!(
                                    subject = param_rx.as_str(),
                                    "subscribing on parameter subject"
                                );
                                nats.subscribe(param_rx.clone())
                                    .await
                                    .context("failed to subscribe on parameter subject")
                            },
                            try_join_all(paths.iter().map(|path| async {
                                let subject = subscribe_path(&param_rx, path.as_ref());
                                trace!(?subject, "subscribing on nested parameter subject");
                                nats.subscribe(Subject::from(subject))
                                    .await
                                    .context("failed to subscribe on nested parameter subject")
                            }))
                        )?;
                        let nested: SubscriberTree = zip(paths.iter(), nested).collect();
                        ensure!(
                            paths.is_empty() == nested.is_empty(),
                            "failed to construct subscription tree"
                        );
                        trace!("publishing handshake response");
                        nats.publish_with_reply(tx.clone(), rx, Bytes::default())
                            .await
                            .context("failed to publish handshake accept")?;
                        Ok((
                            headers,
                            SubjectWriter::new((*nats).clone(), Subject::from(result_subject(&tx))),
                            Reader {
                                buffer: payload,
                                incoming: param_rx,
                                nested: Arc::new(std::sync::Mutex::new(nested)),
                            },
                        ))
                    }
                }
            },
        ))
    }
}
