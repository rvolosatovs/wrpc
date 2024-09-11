use core::future::Future;
use core::pin::pin;
use core::time::Duration;

use anyhow::Context as _;
use bytes::{Bytes, BytesMut};
use futures::TryStreamExt as _;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::{select, try_join};
use tokio_util::codec::{Encoder as _, FramedRead};
use tracing::{debug, instrument, trace, Instrument as _};

use crate::{Deferred as _, Index, TupleDecode, TupleEncode};

/// Client-side handle to a wRPC transport
pub trait Invoke: Send + Sync {
    /// Transport-specific invocation context
    type Context: Send + Sync;

    /// Outgoing multiplexed byte stream
    type Outgoing: AsyncWrite + Index<Self::Outgoing> + Send + Sync + Unpin + 'static;

    /// Incoming multiplexed byte stream
    type Incoming: AsyncRead + Index<Self::Incoming> + Send + Sync + Unpin + 'static;

    /// Invoke function `func` on instance `instance`
    ///
    /// Note, that compilation of code calling methods on [`Invoke`] implementations within [`Send`] async functions
    /// may fail with hard-to-debug errors due to a compiler bug:
    /// [https://github.com/rust-lang/rust/issues/96865](https://github.com/rust-lang/rust/issues/96865)
    ///
    /// The following fails to compile with rustc 1.78.0:
    ///
    /// ```compile_fail
    /// use core::future::Future;
    ///
    /// fn invoke_send<T>() -> impl Future<Output = anyhow::Result<(T::Outgoing, T::Incoming)>> + Send
    /// where
    ///     T: wrpc_transport::Invoke<Context = ()> + Default,
    /// {
    ///     async { T::default().invoke((), "compiler-bug", "free", "since".into(), [[Some(2024)].as_slice(); 0]).await }
    /// }
    /// ```
    ///
    /// ```text
    /// async { T::default().invoke((), "compiler-bug", "free", "since".into(), [[Some(2024)].as_slice(); 0]).await }
    /// |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ implementation of `AsRef` is not general enough
    ///  |
    ///  = note: `[&'0 [Option<usize>]; 0]` must implement `AsRef<[&'1 [Option<usize>]]>`, for any two lifetimes `'0` and `'1`...
    ///  = note: ...but it actually implements `AsRef<[&[Option<usize>]]>`
    /// ```
    ///
    /// The fix is to call [`send`](send_future::SendFuture::send) provided by [`send_future::SendFuture`], re-exported by this crate, on the future before awaiting:
    /// ```
    /// use core::future::Future;
    /// use wrpc_transport::SendFuture as _;
    ///
    /// fn invoke_send<T>() -> impl Future<Output = anyhow::Result<(T::Outgoing, T::Incoming)>> + Send
    /// where
    ///     T: wrpc_transport::Invoke<Context = ()> + Default,
    /// {
    ///     async { T::default().invoke((), "compiler-bug", "free", "since".into(), [[Some(2024)].as_slice(); 0]).send().await }
    /// }
    /// ```

    fn invoke<P>(
        &self,
        cx: Self::Context,
        instance: &str,
        func: &str,
        params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> impl Future<Output = anyhow::Result<(Self::Outgoing, Self::Incoming)>> + Send
    where
        P: AsRef<[Option<usize>]> + Send + Sync;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timeout<'a, T: ?Sized> {
    pub inner: &'a T,
    pub timeout: Duration,
}

impl<T: Invoke> Invoke for Timeout<'_, T> {
    type Context = T::Context;
    type Outgoing = T::Outgoing;
    type Incoming = T::Incoming;


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
        tokio::time::timeout(
            self.timeout,
            self.inner.invoke(cx, instance, func, params, paths),
        )
        .await
        .context("invocation timed out")?
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeoutOwned<T> {
    pub inner: T,
    pub timeout: Duration,
}

impl<T: Invoke> Invoke for TimeoutOwned<T> {
    type Context = T::Context;
    type Outgoing = T::Outgoing;
    type Incoming = T::Incoming;


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
        self.inner
            .timeout(self.timeout)
            .invoke(cx, instance, func, params, paths)
            .await
    }
}

pub trait InvokeExt: Invoke {
    /// Invoke function `func` on instance `instance` using typed `Params` and `Results`

    fn invoke_values<P, Params, Results>(
        &self,
        cx: Self::Context,
        instance: &str,
        func: &str,
        params: Params,
        paths: impl AsRef<[P]> + Send,
    ) -> impl Future<
        Output = anyhow::Result<(
            Results,
            Option<impl Future<Output = anyhow::Result<()>> + Send + 'static>,
        )>,
    > + Send
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
        Params: TupleEncode<Self::Outgoing> + Send,
        Results: TupleDecode<Self::Incoming> + Send,
        <Params::Encoder as tokio_util::codec::Encoder<Params>>::Error:
            std::error::Error + Send + Sync + 'static,
        <Results::Decoder as tokio_util::codec::Decoder>::Error:
            std::error::Error + Send + Sync + 'static,
    {
        async {
            let mut buf = BytesMut::default();
            let mut enc = Params::Encoder::default();
            trace!("encoding parameters");
            enc.encode(params, &mut buf)
                .context("failed to encode parameters")?;
            debug!("invoking function");
            let (mut outgoing, incoming) = self
                .invoke(cx, instance, func, buf.freeze(), paths)
                .await
                .context("failed to invoke function")?;
            outgoing
                .shutdown()
                .await
                .context("failed to shutdown synchronous parameter channel")?;
            let mut tx = enc.take_deferred().map(|tx| {
                tokio::spawn(
                    async {
                        debug!("transmitting async parameters");
                        tx(outgoing.into(), Vec::with_capacity(8))
                            .await
                            .context("failed to write async parameters")
                    }
                )
            });

            let mut dec = FramedRead::new(incoming, Results::Decoder::default());
            let results = async {
                debug!("receiving sync results");
                dec.try_next()
                    .await
                    .context("failed to receive sync results")?
                    .context("incomplete results")
            };
            let results = if let Some(mut fut) = tx.take() {
                let mut results = pin!(results);
                select! {
                    res = &mut results => {
                        tx = Some(fut);
                        res?
                    }
                    res = &mut fut => {
                        res??;
                        results.await?
                    }
                }
            } else {
                results.await?
            };
            trace!("received sync results");
            let rx = dec.decoder_mut().take_deferred();
            Ok((
                results,
                (tx.is_some() || rx.is_some()).then_some(
                    async {
                        match (tx, rx) {
                            (Some(tx), Some(rx)) => {
                                try_join!(
                                    async {
                                        debug!("receiving async results");
                                        rx(dec.into_inner().into(), Vec::with_capacity(8))
                                            .await
                                            .context("receiving async results failed")
                                    },
                                    async {
                                        tx.await.context("transmitting async parameters failed")?
                                    }
                                )?;
                            }
                            (Some(tx), None) => {
                                tx.await.context("transmitting async parameters failed")??;
                            }
                            (None, Some(rx)) => {
                                debug!("receiving async results");
                                rx(dec.into_inner().into(), Vec::with_capacity(8))
                                    .await
                                    .context("receiving async results failed")?;
                            }
                            _ => {}
                        }
                        Ok(())
                    }
                ),
            ))
        }
    }

    /// Invoke function `func` on instance `instance` using typed `Params` and `Results`
    /// This is like [`Self::invoke_values`], but it only results once all I/O is done

    fn invoke_values_blocking<P, Params, Results>(
        &self,
        cx: Self::Context,
        instance: &str,
        func: &str,
        params: Params,
        paths: impl AsRef<[P]> + Send,
    ) -> impl Future<Output = anyhow::Result<Results>> + Send
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
        Params: TupleEncode<Self::Outgoing> + Send,
        Results: TupleDecode<Self::Incoming> + Send,
        <Params::Encoder as tokio_util::codec::Encoder<Params>>::Error:
            std::error::Error + Send + Sync + 'static,
        <Results::Decoder as tokio_util::codec::Decoder>::Error:
            std::error::Error + Send + Sync + 'static,
    {
        async {
            let (ret, io) = self
                .invoke_values(cx, instance, func, params, paths)
                .await?;
            if let Some(io) = io {
                io.await?;
            }
            Ok(ret)
        }
    }

    /// Returns a [`Timeout`], wrapping [Self] with an implementation of [Invoke], which will
    /// error, if call to [`Invoke::invoke`] does not return within a supplied `timeout`
    fn timeout(&self, timeout: Duration) -> Timeout<'_, Self> {
        Timeout {
            inner: self,
            timeout,
        }
    }

    /// This is like [`InvokeExt::timeout`], but moves [Self] and returns corresponding [`TimeoutOwned`]
    fn timeout_owned(self, timeout: Duration) -> TimeoutOwned<Self>
    where
        Self: Sized,
    {
        TimeoutOwned {
            inner: self,
            timeout,
        }
    }
}

impl<T: Invoke> InvokeExt for T {}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::Pin;

    use std::sync::Arc;

    use bytes::Bytes;
    use futures::{Stream, StreamExt as _};
    use send_future::SendFuture as _;

    use super::*;

    #[allow(clippy::manual_async_fn)]
    fn invoke_values_send<T>() -> impl Future<
        Output = anyhow::Result<(
            Pin<Box<dyn Stream<Item = Vec<Pin<Box<dyn Future<Output = String> + Send>>>> + Send>>,
        )>,
    > + Send
    where
        T: Invoke<Context = ()> + Default,
    {
        async {
            let wrpc = T::default();
            let ((r0,), _) = wrpc
                .invoke_values(
                    (),
                    "wrpc-test:integration/async",
                    "with-streams",
                    (),
                    [[None].as_slice()],
                )
                .send()
                .await?;
            Ok(r0)
        }
    }

    async fn call_invoke<T: Invoke>(
        i: &T,
        cx: T::Context,
        paths: Arc<[Arc<[Option<usize>]>]>,
    ) -> anyhow::Result<(T::Outgoing, T::Incoming)> {
        i.invoke(cx, "foo", "bar", Bytes::default(), &paths).await
    }

    async fn call_invoke_async<T>() -> anyhow::Result<(Pin<Box<dyn Stream<Item = Bytes> + Send>>,)>
    where
        T: Invoke<Context = ()> + Default,
    {
        let wrpc = T::default();
        let ((r0,), _) = wrpc
            .invoke_values(
                (),
                "wrpc-test:integration/async",
                "with-streams",
                (),
                [
                    [Some(1), Some(2)].as_slice(),
                    [None].as_slice(),
                    [Some(42)].as_slice(),
                ],
            )
            .await?;
        Ok(r0)
    }

    trait Handler {
        fn foo() -> impl Future<Output = anyhow::Result<()>>;
    }

    impl<T> Handler for T
    where
        T: Invoke<Context = ()> + Default,
    {
        async fn foo() -> anyhow::Result<()> {
            let (st,) = call_invoke_async::<Self>().await?;
            st.collect::<Vec<_>>().await;
            Ok(())
        }
    }
}
