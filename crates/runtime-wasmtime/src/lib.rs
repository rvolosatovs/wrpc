#![allow(clippy::type_complexity)] // TODO: https://github.com/bytecodealliance/wrpc/issues/2

use core::future::Future;
use core::iter::zip;
use core::ops::{BitOrAssign, Shl};
use core::pin::{pin, Pin};
use core::time::Duration;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use bytes::{BufMut as _, Bytes, BytesMut};
use futures::future::try_join_all;
use futures::stream::FuturesUnordered;
use futures::{Stream, TryStreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio::try_join;
use tokio_util::codec::{Encoder, FramedRead};
use tokio_util::compat::FuturesAsyncReadCompatExt as _;
use tracing::{debug, error, instrument, trace, warn, Instrument as _, Span};
use uuid::Uuid;
use wasm_tokio::cm::AsyncReadValue as _;
use wasm_tokio::{
    AsyncReadCore as _, AsyncReadLeb128 as _, AsyncReadUtf8 as _, CoreNameEncoder,
    CoreVecEncoderBytes, Leb128Encoder, Utf8Codec,
};
use wasmtime::component::types::{self, Case, Field};
use wasmtime::component::{
    Func, Instance, InstancePre, LinkerInstance, ResourceAny, ResourceType, Type, Val,
};
use wasmtime::{AsContextMut, Engine, StoreContextMut};
use wasmtime_wasi::pipe::AsyncReadStream;
use wasmtime_wasi::{InputStream, StreamError, WasiView};
use wrpc_transport::{Index as _, Invoke, InvokeExt as _, ListDecoderU8};

// this returns the RPC name for a wasmtime function name.
// Unfortunately, the [`types::ComponentFunc`] does not include the kind information and we want to
// avoid (re-)parsing the WIT here.
fn rpc_func_name(name: &str) -> &str {
    if let Some(name) = name.strip_prefix("[constructor]") {
        name
    } else if let Some(name) = name.strip_prefix("[static]") {
        name
    } else if let Some(name) = name.strip_prefix("[method]") {
        name
    } else {
        name
    }
}

pub struct RemoteResource(pub Bytes);

pub struct ValEncoder<'a, T, W> {
    pub store: StoreContextMut<'a, T>,
    pub ty: &'a Type,
    pub resources: &'a [ResourceType],
    pub deferred: Option<
        Box<dyn FnOnce(W) -> Pin<Box<dyn Future<Output = wasmtime::Result<()>> + Send>> + Send>,
    >,
}

impl<T, W> ValEncoder<'_, T, W> {
    #[must_use]
    pub fn new<'a>(
        store: StoreContextMut<'a, T>,
        ty: &'a Type,
        resources: &'a [ResourceType],
    ) -> ValEncoder<'a, T, W> {
        ValEncoder {
            store,
            ty,
            resources,
            deferred: None,
        }
    }

    pub fn with_type<'a>(&'a mut self, ty: &'a Type) -> ValEncoder<'a, T, W> {
        ValEncoder {
            store: self.store.as_context_mut(),
            ty,
            resources: self.resources,
            deferred: None,
        }
    }
}

fn find_enum_discriminant<'a, T>(
    iter: impl IntoIterator<Item = T>,
    names: impl IntoIterator<Item = &'a str>,
    discriminant: &str,
) -> wasmtime::Result<T> {
    zip(iter, names)
        .find_map(|(i, name)| (name == discriminant).then_some(i))
        .context("unknown enum discriminant")
}

fn find_variant_discriminant<'a, T>(
    iter: impl IntoIterator<Item = T>,
    cases: impl IntoIterator<Item = Case<'a>>,
    discriminant: &str,
) -> wasmtime::Result<(T, Option<Type>)> {
    zip(iter, cases)
        .find_map(|(i, Case { name, ty })| (name == discriminant).then_some((i, ty)))
        .context("unknown variant discriminant")
}

#[inline]
fn flag_bits<'a, T: BitOrAssign + Shl<u8, Output = T> + From<u8>>(
    names: impl IntoIterator<Item = &'a str>,
    flags: impl IntoIterator<Item = &'a str>,
) -> T {
    let mut v = T::from(0);
    let flags: HashSet<&str> = flags.into_iter().collect();
    for (i, name) in zip(0u8.., names) {
        if flags.contains(name) {
            v |= T::from(1) << i;
        }
    }
    v
}

async fn write_deferred<W, I>(w: W, deferred: I) -> wasmtime::Result<()>
where
    W: wrpc_transport::Index<W> + Sync + Send + 'static,
    I: IntoIterator,
    I::IntoIter: ExactSizeIterator<
        Item = Option<
            Box<dyn FnOnce(W) -> Pin<Box<dyn Future<Output = wasmtime::Result<()>> + Send>> + Send>,
        >,
    >,
{
    let futs: FuturesUnordered<_> = zip(0.., deferred)
        .filter_map(|(i, f)| f.map(|f| (w.index(&[i]), f)))
        .map(|(w, f)| async move {
            let w = w?;
            f(w).await
        })
        .collect();
    futs.try_collect().await?;
    Ok(())
}

impl<T, W> Encoder<&Val> for ValEncoder<'_, T, W>
where
    T: WasiView + WrpcView,
    W: AsyncWrite + wrpc_transport::Index<W> + Sync + Send + 'static,
{
    type Error = wasmtime::Error;

    #[allow(clippy::too_many_lines)]

    fn encode(&mut self, v: &Val, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match (v, self.ty) {
            (Val::Bool(v), Type::Bool) => {
                dst.reserve(1);
                dst.put_u8((*v).into());
                Ok(())
            }
            (Val::S8(v), Type::S8) => {
                dst.reserve(1);
                dst.put_i8(*v);
                Ok(())
            }
            (Val::U8(v), Type::U8) => {
                dst.reserve(1);
                dst.put_u8(*v);
                Ok(())
            }
            (Val::S16(v), Type::S16) => Leb128Encoder
                .encode(*v, dst)
                .context("failed to encode s16"),
            (Val::U16(v), Type::U16) => Leb128Encoder
                .encode(*v, dst)
                .context("failed to encode u16"),
            (Val::S32(v), Type::S32) => Leb128Encoder
                .encode(*v, dst)
                .context("failed to encode s32"),
            (Val::U32(v), Type::U32) => Leb128Encoder
                .encode(*v, dst)
                .context("failed to encode u32"),
            (Val::S64(v), Type::S64) => Leb128Encoder
                .encode(*v, dst)
                .context("failed to encode s64"),
            (Val::U64(v), Type::U64) => Leb128Encoder
                .encode(*v, dst)
                .context("failed to encode u64"),
            (Val::Float32(v), Type::Float32) => {
                dst.reserve(4);
                dst.put_f32_le(*v);
                Ok(())
            }
            (Val::Float64(v), Type::Float64) => {
                dst.reserve(8);
                dst.put_f64_le(*v);
                Ok(())
            }
            (Val::Char(v), Type::Char) => {
                Utf8Codec.encode(*v, dst).context("failed to encode char")
            }
            (Val::String(v), Type::String) => CoreNameEncoder
                .encode(v.as_str(), dst)
                .context("failed to encode string"),
            (Val::List(vs), Type::List(ty)) => {
                let ty = ty.ty();
                let n = u32::try_from(vs.len()).context("list length does not fit in u32")?;
                dst.reserve(5 + vs.len());
                Leb128Encoder
                    .encode(n, dst)
                    .context("failed to encode list length")?;
                let mut deferred = Vec::with_capacity(vs.len());
                for v in vs {
                    let mut enc = self.with_type(&ty);
                    enc.encode(v, dst)
                        .context("failed to encode list element")?;
                    deferred.push(enc.deferred);
                }
                if deferred.iter().any(Option::is_some) {
                    self.deferred = Some(Box::new(|w| Box::pin(write_deferred(w, deferred))));
                }
                Ok(())
            }
            (Val::Record(vs), Type::Record(ty)) => {
                dst.reserve(vs.len());
                let mut deferred = Vec::with_capacity(vs.len());
                for ((name, v), Field { ref ty, .. }) in zip(vs, ty.fields()) {
                    let mut enc = self.with_type(ty);
                    enc.encode(v, dst)
                        .with_context(|| format!("failed to encode `{name}` field"))?;
                    deferred.push(enc.deferred);
                }
                if deferred.iter().any(Option::is_some) {
                    self.deferred = Some(Box::new(|w| Box::pin(write_deferred(w, deferred))));
                }
                Ok(())
            }
            (Val::Tuple(vs), Type::Tuple(ty)) => {
                dst.reserve(vs.len());
                let mut deferred = Vec::with_capacity(vs.len());
                for (v, ref ty) in zip(vs, ty.types()) {
                    let mut enc = self.with_type(ty);
                    enc.encode(v, dst)
                        .context("failed to encode tuple element")?;
                    deferred.push(enc.deferred);
                }
                if deferred.iter().any(Option::is_some) {
                    self.deferred = Some(Box::new(|w| Box::pin(write_deferred(w, deferred))));
                }
                Ok(())
            }
            (Val::Variant(discriminant, v), Type::Variant(ty)) => {
                let cases = ty.cases();
                let ty = match cases.len() {
                    ..=0x0000_00ff => {
                        let (discriminant, ty) =
                            find_variant_discriminant(0u8.., cases, discriminant)?;
                        dst.reserve(2 + usize::from(v.is_some()));
                        Leb128Encoder.encode(discriminant, dst)?;
                        ty
                    }
                    0x0000_0100..=0x0000_ffff => {
                        let (discriminant, ty) =
                            find_variant_discriminant(0u16.., cases, discriminant)?;
                        dst.reserve(3 + usize::from(v.is_some()));
                        Leb128Encoder.encode(discriminant, dst)?;
                        ty
                    }
                    0x0001_0000..=0x00ff_ffff => {
                        let (discriminant, ty) =
                            find_variant_discriminant(0u32.., cases, discriminant)?;
                        dst.reserve(4 + usize::from(v.is_some()));
                        Leb128Encoder.encode(discriminant, dst)?;
                        ty
                    }
                    0x0100_0000..=0xffff_ffff => {
                        let (discriminant, ty) =
                            find_variant_discriminant(0u32.., cases, discriminant)?;
                        dst.reserve(5 + usize::from(v.is_some()));
                        Leb128Encoder.encode(discriminant, dst)?;
                        ty
                    }
                    0x1_0000_0000.. => bail!("case count does not fit in u32"),
                };
                if let Some(v) = v {
                    let ty = ty.context("type missing for variant")?;
                    let mut enc = self.with_type(&ty);
                    enc.encode(v, dst)
                        .context("failed to encode variant value")?;
                    if let Some(f) = enc.deferred {
                        self.deferred = Some(f);
                    }
                }
                Ok(())
            }
            (Val::Enum(discriminant), Type::Enum(ty)) => {
                let names = ty.names();
                match names.len() {
                    ..=0x0000_00ff => {
                        let discriminant = find_enum_discriminant(0u8.., names, discriminant)?;
                        dst.reserve(2);
                        Leb128Encoder.encode(discriminant, dst)?;
                    }
                    0x0000_0100..=0x0000_ffff => {
                        let discriminant = find_enum_discriminant(0u16.., names, discriminant)?;
                        dst.reserve(3);
                        Leb128Encoder.encode(discriminant, dst)?;
                    }
                    0x0001_0000..=0x00ff_ffff => {
                        let discriminant = find_enum_discriminant(0u32.., names, discriminant)?;
                        dst.reserve(4);
                        Leb128Encoder.encode(discriminant, dst)?;
                    }
                    0x0100_0000..=0xffff_ffff => {
                        let discriminant = find_enum_discriminant(0u32.., names, discriminant)?;
                        dst.reserve(5);
                        Leb128Encoder.encode(discriminant, dst)?;
                    }
                    0x1_0000_0000.. => bail!("name count does not fit in u32"),
                }
                Ok(())
            }
            (Val::Option(None), Type::Option(_)) => {
                dst.reserve(1);
                dst.put_u8(0);
                Ok(())
            }
            (Val::Option(Some(v)), Type::Option(ty)) => {
                dst.reserve(2);
                dst.put_u8(1);
                let ty = ty.ty();
                let mut enc = self.with_type(&ty);
                enc.encode(v, dst)
                    .context("failed to encode `option::some` value")?;
                if let Some(f) = enc.deferred {
                    self.deferred = Some(f);
                }
                Ok(())
            }
            (Val::Result(v), Type::Result(ty)) => match v {
                Ok(v) => match (v, ty.ok()) {
                    (Some(v), Some(ty)) => {
                        dst.reserve(2);
                        dst.put_u8(0);
                        let mut enc = self.with_type(&ty);
                        enc.encode(v, dst)
                            .context("failed to encode `result::ok` value")?;
                        if let Some(f) = enc.deferred {
                            self.deferred = Some(f);
                        }
                        Ok(())
                    }
                    (Some(_v), None) => bail!("`result::ok` value of unknown type"),
                    (None, Some(_ty)) => bail!("`result::ok` value missing"),
                    (None, None) => {
                        dst.reserve(1);
                        dst.put_u8(0);
                        Ok(())
                    }
                },
                Err(v) => match (v, ty.err()) {
                    (Some(v), Some(ty)) => {
                        dst.reserve(2);
                        dst.put_u8(1);
                        let mut enc = self.with_type(&ty);
                        enc.encode(v, dst)
                            .context("failed to encode `result::err` value")?;
                        if let Some(f) = enc.deferred {
                            self.deferred = Some(f);
                        }
                        Ok(())
                    }
                    (Some(_v), None) => bail!("`result::err` value of unknown type"),
                    (None, Some(_ty)) => bail!("`result::err` value missing"),
                    (None, None) => {
                        dst.reserve(1);
                        dst.put_u8(1);
                        Ok(())
                    }
                },
            },
            (Val::Flags(vs), Type::Flags(ty)) => {
                let names = ty.names();
                let vs = vs.iter().map(String::as_str);
                match names.len() {
                    ..=8 => {
                        dst.reserve(1);
                        dst.put_u8(flag_bits(names, vs));
                    }
                    9..=16 => {
                        dst.reserve(2);
                        dst.put_u16_le(flag_bits(names, vs));
                    }
                    17..=24 => {
                        dst.reserve(3);
                        dst.put_slice(&u32::to_le_bytes(flag_bits(names, vs))[..3]);
                    }
                    25..=32 => {
                        dst.reserve(4);
                        dst.put_u32_le(flag_bits(names, vs));
                    }
                    33..=40 => {
                        dst.reserve(5);
                        dst.put_slice(&u64::to_le_bytes(flag_bits(names, vs))[..5]);
                    }
                    41..=48 => {
                        dst.reserve(6);
                        dst.put_slice(&u64::to_le_bytes(flag_bits(names, vs))[..6]);
                    }
                    49..=56 => {
                        dst.reserve(7);
                        dst.put_slice(&u64::to_le_bytes(flag_bits(names, vs))[..7]);
                    }
                    57..=64 => {
                        dst.reserve(8);
                        dst.put_u64_le(flag_bits(names, vs));
                    }
                    65..=72 => {
                        dst.reserve(9);
                        dst.put_slice(&u128::to_le_bytes(flag_bits(names, vs))[..9]);
                    }
                    73..=80 => {
                        dst.reserve(10);
                        dst.put_slice(&u128::to_le_bytes(flag_bits(names, vs))[..10]);
                    }
                    81..=88 => {
                        dst.reserve(11);
                        dst.put_slice(&u128::to_le_bytes(flag_bits(names, vs))[..11]);
                    }
                    89..=96 => {
                        dst.reserve(12);
                        dst.put_slice(&u128::to_le_bytes(flag_bits(names, vs))[..12]);
                    }
                    97..=104 => {
                        dst.reserve(13);
                        dst.put_slice(&u128::to_le_bytes(flag_bits(names, vs))[..13]);
                    }
                    105..=112 => {
                        dst.reserve(14);
                        dst.put_slice(&u128::to_le_bytes(flag_bits(names, vs))[..14]);
                    }
                    113..=120 => {
                        dst.reserve(15);
                        dst.put_slice(&u128::to_le_bytes(flag_bits(names, vs))[..15]);
                    }
                    121..=128 => {
                        dst.reserve(16);
                        dst.put_u128_le(flag_bits(names, vs));
                    }
                    bits @ 129.. => {
                        let mut cap = bits / 8;
                        if bits % 8 != 0 {
                            cap = cap.saturating_add(1);
                        }
                        let mut buf = vec![0; cap];
                        let flags: HashSet<&str> = vs.into_iter().collect();
                        for (i, name) in names.enumerate() {
                            if flags.contains(name) {
                                buf[i / 8] |= 1 << (i % 8);
                            }
                        }
                        dst.extend_from_slice(&buf);
                    }
                }
                Ok(())
            }
            (Val::Resource(resource), Type::Own(ty) | Type::Borrow(ty)) => {
                if *ty == ResourceType::host::<InputStream>() {
                    let stream = resource
                        .try_into_resource::<InputStream>(&mut self.store)
                        .context("failed to downcast `wasi:io/input-stream`")?;
                    if stream.owned() {
                        let stream = self
                            .store
                            .data_mut()
                            .table()
                            .delete(stream)
                            .context("failed to delete input stream")?;
                        match stream {
                            InputStream::Host(mut stream) => {
                                self.deferred = Some(Box::new(|w| {
                                    Box::pin(async move {
                                        let mut w = pin!(w);
                                        loop {
                                            stream.ready().await;
                                            match stream.read(8096) {
                                                Ok(buf) => {
                                                    let mut chunk = BytesMut::with_capacity(
                                                        buf.len().saturating_add(5),
                                                    );
                                                    CoreVecEncoderBytes
                                                        .encode(buf, &mut chunk)
                                                        .context(
                                                            "failed to encode input stream chunk",
                                                        )?;
                                                    w.write_all(&chunk).await?;
                                                }
                                                Err(StreamError::Closed) => {
                                                    w.write_all(&[0x00]).await?;
                                                }
                                                Err(err) => return Err(err.into()),
                                            }
                                        }
                                    })
                                }));
                            }
                            InputStream::File(mut stream) => {
                                self.deferred = Some(Box::new(|w| {
                                    Box::pin(async move {
                                        let mut w = pin!(w);
                                        loop {
                                            match stream.read(8096).await {
                                                Ok(buf) => {
                                                    let mut chunk = BytesMut::with_capacity(
                                                        buf.len().saturating_add(5),
                                                    );
                                                    CoreVecEncoderBytes
                                                        .encode(buf, &mut chunk)
                                                        .context(
                                                            "failed to encode input stream chunk",
                                                        )?;
                                                    w.write_all(&chunk).await?;
                                                }
                                                Err(StreamError::Closed) => {
                                                    w.write_all(&[0x00]).await?;
                                                }
                                                Err(err) => return Err(err.into()),
                                            }
                                        }
                                    })
                                }));
                            }
                        }
                    } else {
                        self.store
                            .data_mut()
                            .table()
                            .get_mut(&stream)
                            .context("failed to get input stream")?;
                        // NOTE: In order to handle this we'd need to know how many bytes the
                        // receiver has read. That means that some kind of callback would be required from
                        // the receiver. This is not trivial and generally should be a very rare use case.
                        bail!("encoding borrowed `wasi:io/input-stream` not supported yet");
                    };
                    Ok(())
                } else if resource.ty() == ResourceType::host::<RemoteResource>() {
                    let resource = resource
                        .try_into_resource(&mut self.store)
                        .context("resource type mismatch")?;
                    let table = self.store.data_mut().table();
                    if resource.owned() {
                        let RemoteResource(buf) = table
                            .delete(resource)
                            .context("failed to delete remote resource")?;
                        CoreVecEncoderBytes
                            .encode(buf, dst)
                            .context("failed to encode resource handle")
                    } else {
                        let RemoteResource(buf) = table
                            .get(&resource)
                            .context("failed to get remote resource")?;
                        CoreVecEncoderBytes
                            .encode(buf, dst)
                            .context("failed to encode resource handle")
                    }
                } else if self.resources.contains(ty) {
                    let id = Uuid::now_v7();
                    CoreVecEncoderBytes
                        .encode(id.to_bytes_le().as_slice(), dst)
                        .context("failed to encode resource handle")?;
                    trace!(?id, "store shared resource");
                    if self
                        .store
                        .data_mut()
                        .shared_resources()
                        .0
                        .insert(id, *resource)
                        .is_some()
                    {
                        error!(?id, "duplicate resource ID generated");
                    }
                    Ok(())
                } else {
                    bail!("encoding host resources not supported yet")
                }
            }
            _ => bail!("value type mismatch"),
        }
    }
}

#[inline]
async fn read_flags(n: usize, r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<u128> {
    let mut buf = 0u128.to_le_bytes();
    r.read_exact(&mut buf[..n]).await?;
    Ok(u128::from_le_bytes(buf))
}

/// Read encoded value of type [`Type`] from an [`AsyncRead`] into a [`Val`]

pub async fn read_value<T, R>(
    store: &mut impl AsContextMut<Data = T>,
    r: &mut Pin<&mut R>,
    resources: &[ResourceType],
    val: &mut Val,
    ty: &Type,
    path: &[usize],
) -> std::io::Result<()>
where
    T: WasiView + WrpcView,
    R: AsyncRead + wrpc_transport::Index<R> + Send + Unpin + 'static,
{
    match ty {
        Type::Bool => {
            let v = r.read_bool().await?;
            *val = Val::Bool(v);
            Ok(())
        }
        Type::S8 => {
            let v = r.read_i8().await?;
            *val = Val::S8(v);
            Ok(())
        }
        Type::U8 => {
            let v = r.read_u8().await?;
            *val = Val::U8(v);
            Ok(())
        }
        Type::S16 => {
            let v = r.read_i16_leb128().await?;
            *val = Val::S16(v);
            Ok(())
        }
        Type::U16 => {
            let v = r.read_u16_leb128().await?;
            *val = Val::U16(v);
            Ok(())
        }
        Type::S32 => {
            let v = r.read_i32_leb128().await?;
            *val = Val::S32(v);
            Ok(())
        }
        Type::U32 => {
            let v = r.read_u32_leb128().await?;
            *val = Val::U32(v);
            Ok(())
        }
        Type::S64 => {
            let v = r.read_i64_leb128().await?;
            *val = Val::S64(v);
            Ok(())
        }
        Type::U64 => {
            let v = r.read_u64_leb128().await?;
            *val = Val::U64(v);
            Ok(())
        }
        Type::Float32 => {
            let v = r.read_f32_le().await?;
            *val = Val::Float32(v);
            Ok(())
        }
        Type::Float64 => {
            let v = r.read_f64_le().await?;
            *val = Val::Float64(v);
            Ok(())
        }
        Type::Char => {
            let v = r.read_char_utf8().await?;
            *val = Val::Char(v);
            Ok(())
        }
        Type::String => {
            let mut s = String::default();
            r.read_core_name(&mut s).await?;
            *val = Val::String(s);
            Ok(())
        }
        Type::List(ty) => {
            let n = r.read_u32_leb128().await?;
            let n = n.try_into().unwrap_or(usize::MAX);
            let mut vs = Vec::with_capacity(n);
            let ty = ty.ty();
            let mut path = path.to_vec();
            for i in 0..n {
                let mut v = Val::Bool(false);
                path.push(i);
                trace!(i, "reading list element value");
                Box::pin(read_value(store, r, resources, &mut v, &ty, &path)).await?;
                path.pop();
                vs.push(v);
            }
            *val = Val::List(vs);
            Ok(())
        }
        Type::Record(ty) => {
            let fields = ty.fields();
            let mut vs = Vec::with_capacity(fields.len());
            let mut path = path.to_vec();
            for (i, Field { name, ty }) in fields.enumerate() {
                let mut v = Val::Bool(false);
                path.push(i);
                trace!(i, "reading struct field value");
                Box::pin(read_value(store, r, resources, &mut v, &ty, &path)).await?;
                path.pop();
                vs.push((name.to_string(), v));
            }
            *val = Val::Record(vs);
            Ok(())
        }
        Type::Tuple(ty) => {
            let types = ty.types();
            let mut vs = Vec::with_capacity(types.len());
            let mut path = path.to_vec();
            for (i, ty) in types.enumerate() {
                let mut v = Val::Bool(false);
                path.push(i);
                trace!(i, "reading tuple element value");
                Box::pin(read_value(store, r, resources, &mut v, &ty, &path)).await?;
                path.pop();
                vs.push(v);
            }
            *val = Val::Tuple(vs);
            Ok(())
        }
        Type::Variant(ty) => {
            let discriminant = r.read_u32_leb128().await?;
            let discriminant = discriminant
                .try_into()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            let Case { name, ty } = ty.cases().nth(discriminant).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown variant discriminant `{discriminant}`"),
                )
            })?;
            let name = name.to_string();
            if let Some(ty) = ty {
                let mut v = Val::Bool(false);
                trace!(variant = name, "reading nested variant value");
                Box::pin(read_value(store, r, resources, &mut v, &ty, path)).await?;
                *val = Val::Variant(name, Some(Box::new(v)));
            } else {
                *val = Val::Variant(name, None);
            }
            Ok(())
        }
        Type::Enum(ty) => {
            let discriminant = r.read_u32_leb128().await?;
            let discriminant = discriminant
                .try_into()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            let name = ty.names().nth(discriminant).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown enum discriminant `{discriminant}`"),
                )
            })?;
            *val = Val::Enum(name.to_string());
            Ok(())
        }
        Type::Option(ty) => {
            let ok = r.read_option_status().await?;
            if ok {
                let mut v = Val::Bool(false);
                trace!("reading nested `option::some` value");
                Box::pin(read_value(store, r, resources, &mut v, &ty.ty(), path)).await?;
                *val = Val::Option(Some(Box::new(v)));
            } else {
                *val = Val::Option(None);
            }
            Ok(())
        }
        Type::Result(ty) => {
            let ok = r.read_result_status().await?;
            if ok {
                if let Some(ty) = ty.ok() {
                    let mut v = Val::Bool(false);
                    trace!("reading nested `result::ok` value");
                    Box::pin(read_value(store, r, resources, &mut v, &ty, path)).await?;
                    *val = Val::Result(Ok(Some(Box::new(v))));
                } else {
                    *val = Val::Result(Ok(None));
                }
            } else if let Some(ty) = ty.err() {
                let mut v = Val::Bool(false);
                trace!("reading nested `result::err` value");
                Box::pin(read_value(store, r, resources, &mut v, &ty, path)).await?;
                *val = Val::Result(Err(Some(Box::new(v))));
            } else {
                *val = Val::Result(Err(None));
            }
            Ok(())
        }
        Type::Flags(ty) => {
            let names = ty.names();
            let flags = match names.len() {
                ..=8 => read_flags(1, r).await?,
                9..=16 => read_flags(2, r).await?,
                17..=24 => read_flags(3, r).await?,
                25..=32 => read_flags(4, r).await?,
                33..=40 => read_flags(5, r).await?,
                41..=48 => read_flags(6, r).await?,
                49..=56 => read_flags(7, r).await?,
                57..=64 => read_flags(8, r).await?,
                65..=72 => read_flags(9, r).await?,
                73..=80 => read_flags(10, r).await?,
                81..=88 => read_flags(11, r).await?,
                89..=96 => read_flags(12, r).await?,
                97..=104 => read_flags(13, r).await?,
                105..=112 => read_flags(14, r).await?,
                113..=120 => read_flags(15, r).await?,
                121..=128 => r.read_u128_le().await?,
                bits @ 129.. => {
                    let mut cap = bits / 8;
                    if bits % 8 != 0 {
                        cap = cap.saturating_add(1);
                    }
                    let mut buf = vec![0; cap];
                    r.read_exact(&mut buf).await?;
                    let mut vs = Vec::with_capacity(
                        buf.iter()
                            .map(|b| b.count_ones())
                            .sum::<u32>()
                            .try_into()
                            .unwrap_or(usize::MAX),
                    );
                    for (i, name) in names.enumerate() {
                        if buf[i / 8] & (1 << (i % 8)) != 0 {
                            vs.push(name.to_string());
                        }
                    }
                    *val = Val::Flags(vs);
                    return Ok(());
                }
            };
            let mut vs = Vec::with_capacity(flags.count_ones().try_into().unwrap_or(usize::MAX));
            for (i, name) in zip(0.., names) {
                if flags & (1 << i) != 0 {
                    vs.push(name.to_string());
                }
            }
            *val = Val::Flags(vs);
            Ok(())
        }
        Type::Own(ty) | Type::Borrow(ty) => {
            if *ty == ResourceType::host::<InputStream>() {
                let mut store = store.as_context_mut();
                let r = r
                    .index(path)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
                // TODO: Implement a custom reader, this approach ignores the stream end (`\0`),
                // which will could potentially break/hang with some transports
                let res = store
                    .data_mut()
                    .table()
                    .push(InputStream::Host(Box::new(AsyncReadStream::new(
                        FramedRead::new(r, ListDecoderU8::default())
                            .into_async_read()
                            .compat(),
                    ))))
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::OutOfMemory, err))?;
                let v = res
                    .try_into_resource_any(store)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
                *val = Val::Resource(v);
                Ok(())
            } else if resources.contains(ty) {
                let mut store = store.as_context_mut();
                let mut id = uuid::Bytes::default();
                debug_assert_eq!(id.len(), 16);
                let n = r.read_u8_leb128().await?;
                if usize::from(n) != id.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "invalid guest resource handle length {n}, expected {}",
                            id.len()
                        ),
                    ));
                }
                let n = r.read_exact(&mut id).await?;
                if n != id.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "invalid amount of guest resource handle bytes read {n}, expected {}",
                            id.len()
                        ),
                    ));
                }

                let id = Uuid::from_bytes_le(id);
                trace!(?id, "lookup shared resource");
                let resource = store
                    .data_mut()
                    .shared_resources()
                    .0
                    .get(&id)
                    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
                *val = Val::Resource(*resource);
                Ok(())
            } else {
                let mut store = store.as_context_mut();
                let n = r.read_u32_leb128().await?;
                let n = usize::try_from(n)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
                let mut buf = Vec::with_capacity(n);
                r.read_to_end(&mut buf).await?;
                let table = store.data_mut().table();
                let resource = table
                    .push(RemoteResource(buf.into()))
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::OutOfMemory, err))?;
                let resource = resource
                    .try_into_resource_any(store)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
                *val = Val::Resource(resource);
                Ok(())
            }
        }
    }
}

/// A table of shared resources exported by the component
#[derive(Debug, Default)]
pub struct SharedResourceTable(HashMap<Uuid, ResourceAny>);

pub trait WrpcView: Send {
    type Invoke: Invoke;

    /// Returns an [Invoke] implementation used to satisfy polyfilled imports
    fn client(&self) -> &Self::Invoke;

    /// Returns a table of shared exported resources
    fn shared_resources(&mut self) -> &mut SharedResourceTable;

    /// Optional invocation timeout, component will trap if invocation is not finished within the
    /// returned [Duration]. If this method returns [None], then no timeout will be used.
    fn timeout(&self) -> Option<Duration> {
        None
    }
}

/// Polyfill [`types::ComponentItem`] in a [`LinkerInstance`] using [`wrpc_transport::Invoke`]

pub fn link_item<V>(
    engine: &Engine,
    linker: &mut LinkerInstance<V>,
    resources: impl Into<Arc<[ResourceType]>>,
    ty: types::ComponentItem,
    instance: impl Into<Arc<str>>,
    name: impl Into<Arc<str>>,
    cx: <V::Invoke as Invoke>::Context,
) -> wasmtime::Result<()>
where
    V: WasiView + WrpcView,
    <V::Invoke as Invoke>::Context: Clone + 'static,
{
    let instance = instance.into();
    let resources = resources.into();
    match ty {
        types::ComponentItem::ComponentFunc(ty) => {
            let name = name.into();
            debug!(?instance, ?name, "linking function");
            link_function(linker, Arc::clone(&resources), ty, instance, name, cx)?;
        }
        types::ComponentItem::CoreFunc(_) => {
            bail!("polyfilling core functions not supported yet")
        }
        types::ComponentItem::Module(_) => bail!("polyfilling modules not supported yet"),
        types::ComponentItem::Component(ty) => {
            for (name, ty) in ty.imports(engine) {
                debug!(?instance, name, "linking component item");
                link_item(
                    engine,
                    linker,
                    Arc::clone(&resources),
                    ty,
                    "",
                    name,
                    cx.clone(),
                )?;
            }
        }
        types::ComponentItem::ComponentInstance(ty) => {
            let name = name.into();
            let mut linker = linker
                .instance(&name)
                .with_context(|| format!("failed to instantiate `{name}` in the linker"))?;
            debug!(?instance, ?name, "linking instance");
            link_instance(engine, &mut linker, resources, ty, name, cx)?;
        }
        types::ComponentItem::Type(_) => {}
        types::ComponentItem::Resource(_) => {
            let name = name.into();
            debug!(?instance, ?name, "linking resource");
            linker.resource(&name, ResourceType::host::<RemoteResource>(), |_, _| Ok(()))?;
        }
    }
    Ok(())
}

/// Polyfill [`types::ComponentInstance`] in a [`LinkerInstance`] using [`wrpc_transport::Invoke`]

pub fn link_instance<V>(
    engine: &Engine,
    linker: &mut LinkerInstance<V>,
    resources: impl Into<Arc<[ResourceType]>>,
    ty: types::ComponentInstance,
    name: impl Into<Arc<str>>,
    cx: <V::Invoke as Invoke>::Context,
) -> wasmtime::Result<()>
where
    V: WrpcView + WasiView,
    <V::Invoke as Invoke>::Context: Clone + 'static,
{
    let instance = name.into();
    let resources = resources.into();
    for (name, ty) in ty.exports(engine) {
        debug!(name, "linking instance item");
        link_item(
            engine,
            linker,
            Arc::clone(&resources),
            ty,
            Arc::clone(&instance),
            name,
            cx.clone(),
        )?;
    }
    Ok(())
}

/// Polyfill [`types::ComponentFunc`] in a [`LinkerInstance`] using [`wrpc_transport::Invoke`]

pub fn link_function<V>(
    linker: &mut LinkerInstance<V>,
    resources: impl Into<Arc<[ResourceType]>>,
    ty: types::ComponentFunc,
    instance: impl Into<Arc<str>>,
    name: impl Into<Arc<str>>,
    cx: <V::Invoke as Invoke>::Context,
) -> wasmtime::Result<()>
where
    V: WrpcView + WasiView,
    <V::Invoke as Invoke>::Context: Clone + 'static,
{
    let span = Span::current();
    let instance = instance.into();
    let name = name.into();
    let resources = resources.into();
    linker.func_new_async(&Arc::clone(&name), move |mut store, params, results| {
        let cx = cx.clone();
        let ty = ty.clone();
        let instance = Arc::clone(&instance);
        let name = Arc::clone(&name);
        let resources = Arc::clone(&resources);
        Box::new(
            async move {
                let mut buf = BytesMut::default();
                let mut deferred = vec![];
                for (v, ref ty) in zip(params, ty.params()) {
                    let mut enc = ValEncoder::new(store.as_context_mut(), ty, &resources);
                    enc.encode(v, &mut buf)
                        .context("failed to encode parameter")?;
                    deferred.push(enc.deferred);
                }
                let clt = store.data().client();
                let timeout = store.data().timeout();
                let buf = buf.freeze();
                // TODO: set paths
                let paths = &[[]; 0];
                let rpc_name = rpc_func_name(&name);
                let start = Instant::now();
                let (outgoing, incoming) = if let Some(timeout) = timeout {
                    clt.timeout(timeout)
                        .invoke(cx, &instance, rpc_name, buf, paths)
                        .await
                } else {
                    clt.invoke(cx, &instance, rpc_name, buf, paths).await
                }
                .with_context(|| {
                    format!("failed to invoke `{instance}.{name}` polyfill via wRPC")
                })?;
                let tx = async {
                    try_join_all(
                        zip(0.., deferred)
                            .filter_map(|(i, f)| f.map(|f| (outgoing.index(&[i]), f)))
                            .map(|(w, f)| async move {
                                let w = w?;
                                f(w).await
                            }),
                    )
                    .await
                    .context("failed to write asynchronous parameters")?;
                    let mut outgoing = pin!(outgoing);
                    outgoing
                        .flush()
                        .await
                        .context("failed to flush outgoing stream")?;
                    if let Err(err) = outgoing.shutdown().await {
                        trace!(?err, "failed to shutdown outgoing stream")
                    }
                    anyhow::Ok(())
                };
                let rx = async {
                    let mut incoming = pin!(incoming);
                    for (i, (v, ref ty)) in zip(results, ty.results()).enumerate() {
                        read_value(&mut store, &mut incoming, &resources, v, ty, &[i])
                            .await
                            .with_context(|| format!("failed to decode return value {i}"))?;
                    }
                    Ok(())
                };
                if let Some(timeout) = timeout {
                    let timeout =
                        timeout.saturating_sub(Instant::now().saturating_duration_since(start));
                    try_join!(
                        async {
                            tokio::time::timeout(timeout, tx)
                                .await
                                .context("data transmission timed out")?
                        },
                        async {
                            tokio::time::timeout(timeout, rx)
                                .await
                                .context("data receipt timed out")?
                        },
                    )?;
                } else {
                    try_join!(tx, rx)?;
                }
                Ok(())
            }

        )
    })
}

pub async fn call<C, I, O>(
    mut store: C,
    rx: I,
    mut tx: O,
    params_ty: impl ExactSizeIterator<Item = &Type>,
    results_ty: impl ExactSizeIterator<Item = &Type>,
    func: Func,
    guest_resources: &[ResourceType],
) -> anyhow::Result<()>
where
    I: AsyncRead + wrpc_transport::Index<I> + Send + Sync + Unpin + 'static,
    O: AsyncWrite + wrpc_transport::Index<O> + Send + Sync + Unpin + 'static,
    C: AsContextMut,
    C::Data: WasiView + WrpcView,
{
    let mut params = vec![Val::Bool(false); params_ty.len()];
    let mut rx = pin!(rx);
    for (i, (v, ty)) in zip(&mut params, params_ty).enumerate() {
        read_value(&mut store, &mut rx, guest_resources, v, ty, &[i])
            .await
            .with_context(|| format!("failed to decode parameter value {i}"))?;
    }
    let mut results = vec![Val::Bool(false); results_ty.len()];
    func.call_async(&mut store, &params, &mut results)
        .await
        .context("failed to call function")?;
    let mut buf = BytesMut::default();
    let mut deferred = vec![];
    for (i, (ref v, ty)) in zip(results, results_ty).enumerate() {
        let mut enc = ValEncoder::new(store.as_context_mut(), ty, guest_resources);
        enc.encode(v, &mut buf)
            .with_context(|| format!("failed to encode result value {i}"))?;
        deferred.push(enc.deferred);
    }
    debug!("transmitting results");
    tx.write_all(&buf)
        .await
        .context("failed to transmit results")?;
    tx.flush()
        .await
        .context("failed to flush outgoing stream")?;
    if let Err(err) = tx.shutdown().await {
        trace!(?err, "failed to shutdown outgoing stream")
    }
    try_join_all(
        zip(0.., deferred)
            .filter_map(|(i, f)| f.map(|f| (tx.index(&[i]), f)))
            .map(|(w, f)| async move {
                let w = w?;
                f(w).await
            }),
    )
    .await?;
    func.post_return_async(&mut store)
        .await
        .context("failed to perform post-return cleanup")?;
    Ok(())
}

/// Recursively iterates the component item type and collects all exported resource types

pub fn collect_item_resources(
    engine: &Engine,
    ty: types::ComponentItem,
    resources: &mut impl Extend<types::ResourceType>,
) {
    match ty {
        types::ComponentItem::ComponentFunc(_)
        | types::ComponentItem::CoreFunc(_)
        | types::ComponentItem::Module(_)
        | types::ComponentItem::Type(_) => {}
        types::ComponentItem::Component(ty) => collect_component_resources(engine, &ty, resources),
        types::ComponentItem::ComponentInstance(ty) => {
            collect_instance_resources(engine, &ty, resources);
        }
        types::ComponentItem::Resource(ty) => resources.extend([ty]),
    }
}

/// Recursively iterates the component type and collects all exported resource types

pub fn collect_instance_resources(
    engine: &Engine,
    ty: &types::ComponentInstance,
    resources: &mut impl Extend<types::ResourceType>,
) {
    for (_, ty) in ty.exports(engine) {
        collect_item_resources(engine, ty, resources);
    }
}

/// Recursively iterates the component type and collects all exported resource types

pub fn collect_component_resources(
    engine: &Engine,
    ty: &types::Component,
    resources: &mut impl Extend<types::ResourceType>,
) {
    for (_, ty) in ty.exports(engine) {
        collect_item_resources(engine, ty, resources);
    }
}

pub trait ServeExt: wrpc_transport::Serve {
    /// Serve [`types::ComponentFunc`] from an [`InstancePre`] instantiating it on each call.
    /// This serving method does not support guest-exported resources.

    fn serve_function<T>(
        &self,
        store: impl Fn() -> wasmtime::Store<T> + Send + 'static,
        instance_pre: InstancePre<T>,
        ty: types::ComponentFunc,
        instance_name: &str,
        name: &str,
    ) -> impl Future<
        Output = anyhow::Result<
            impl Stream<
                    Item = anyhow::Result<(
                        Self::Context,
                        Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>,
                    )>,
                > + Send
                + 'static,
        >,
    > + Send
    where
        T: WasiView + WrpcView + 'static,
    {
        let span = Span::current();
        async move {
            debug!(instance = instance_name, name, "serving function export");
            let component_ty = instance_pre.component();
            let idx = if instance_name.is_empty() {
                None
            } else {
                let (_, idx) = component_ty
                    .export_index(None, instance_name)
                    .with_context(|| format!("export `{instance_name}` not found"))?;
                Some(idx)
            };
            let (_, idx) = component_ty
                .export_index(idx.as_ref(), name)
                .with_context(|| format!("export `{name}` not found"))?;

            // TODO: set paths
            let invocations = self.serve(instance_name, rpc_func_name(name), []).await?;
            let name = Arc::<str>::from(name);
            let params_ty: Arc<[_]> = ty.params().collect();
            let results_ty: Arc<[_]> = ty.results().collect();
            Ok(invocations.map_ok(move |(cx, tx, rx)| {
                let instance_pre = instance_pre.clone();
                let name = Arc::clone(&name);
                let params_ty = Arc::clone(&params_ty);
                let results_ty = Arc::clone(&results_ty);

                let mut store = store();
                (
                    cx,
                    Box::pin(
                        async move {
                            let instance = instance_pre
                                .instantiate_async(&mut store)
                                .await
                                .context("failed to instantiate component")?;
                            let func = instance
                                .get_func(&mut store, idx)
                                .with_context(|| format!("function export `{name}` not found"))?;
                            call(
                                &mut store,
                                rx,
                                tx,
                                params_ty.iter(),
                                results_ty.iter(),
                                func,
                                &[],
                            )
                            .await
                        }

                    ) as Pin<Box<dyn Future<Output = _> + Send + 'static>>,
                )
            }))
        }
    }

    /// Like [`Self::serve_function`], but with a shared `store` instance.
    /// This is required to allow for serving functions, which operate on guest-exported resources.

    fn serve_function_shared<T>(
        &self,
        store: Arc<Mutex<wasmtime::Store<T>>>,
        instance: Instance,
        guest_resources: impl Into<Arc<[ResourceType]>>,
        ty: types::ComponentFunc,
        instance_name: &str,
        name: &str,
    ) -> impl Future<
        Output = anyhow::Result<
            impl Stream<
                    Item = anyhow::Result<(
                        Self::Context,
                        Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>,
                    )>,
                > + Send
                + 'static,
        >,
    > + Send
    where
        T: WasiView + WrpcView + 'static,
    {
        let span = Span::current();
        let guest_resources = guest_resources.into();
        async move {
            let func = {
                let mut store = store.lock().await;
                let idx = if instance_name.is_empty() {
                    None
                } else {
                    let idx = instance
                        .get_export(store.as_context_mut(), None, instance_name)
                        .with_context(|| format!("export `{instance_name}` not found"))?;
                    Some(idx)
                };
                let idx = instance
                    .get_export(store.as_context_mut(), idx.as_ref(), name)
                    .with_context(|| format!("export `{name}` not found"))?;
                instance.get_func(store.as_context_mut(), idx)
            }
            .with_context(|| format!("function export `{name}` not found"))?;
            debug!(instance = instance_name, name, "serving function export");
            // TODO: set paths
            let invocations = self.serve(instance_name, rpc_func_name(name), []).await?;
            let params_ty: Arc<[_]> = ty.params().collect();
            let results_ty: Arc<[_]> = ty.results().collect();
            let guest_resources = Arc::clone(&guest_resources);
            Ok(invocations.map_ok(move |(cx, tx, rx)| {
                let params_ty = Arc::clone(&params_ty);
                let results_ty = Arc::clone(&results_ty);
                let guest_resources = Arc::clone(&guest_resources);
                let store = Arc::clone(&store);
                (
                    cx,
                    Box::pin(
                        async move {
                            let mut store = store.lock().await;
                            call(
                                &mut *store,
                                rx,
                                tx,
                                params_ty.iter(),
                                results_ty.iter(),
                                func,
                                &guest_resources,
                            )
                            .await?;
                            Ok(())
                        }

                    ) as Pin<Box<dyn Future<Output = _> + Send + 'static>>,
                )
            }))
        }
    }
}

impl<T: wrpc_transport::Serve> ServeExt for T {}
