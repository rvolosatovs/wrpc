use core::any::TypeId;
use core::fmt::{self, Debug};
use core::future::Future;
use core::hash::{Hash, Hasher};
use core::iter::zip;
use core::marker::PhantomData;
use core::mem;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;

use bytes::{Buf as _, BufMut as _, Bytes, BytesMut};
use futures::stream::{self, FuturesUnordered};
use futures::{Stream, StreamExt as _, TryStreamExt as _};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::{select, try_join};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::codec::{Encoder as _, FramedRead};
use tokio_util::io::StreamReader;
use tracing::{instrument, trace};
use wasm_tokio::cm::{
    BoolCodec, F32Codec, F64Codec, OptionDecoder, OptionEncoder, PrimValEncoder, ResultDecoder,
    ResultEncoder, S16Codec, S32Codec, S64Codec, S8Codec, TupleDecoder, TupleEncoder, U16Codec,
    U32Codec, U64Codec, U8Codec,
};
use wasm_tokio::{
    CoreNameDecoder, CoreNameEncoder, CoreVecDecoder, CoreVecDecoderBytes, CoreVecEncoderBytes,
    Leb128DecoderI128, Leb128DecoderI16, Leb128DecoderI32, Leb128DecoderI64, Leb128DecoderI8,
    Leb128DecoderU128, Leb128DecoderU16, Leb128DecoderU32, Leb128DecoderU64, Leb128DecoderU8,
    Leb128Encoder, Utf8Codec,
};

#[repr(transparent)]
pub struct ResourceBorrow<T: ?Sized> {
    repr: Bytes,
    _ty: PhantomData<T>,
}

impl<T: ?Sized> From<Bytes> for ResourceBorrow<T> {
    fn from(repr: Bytes) -> Self {
        Self {
            repr,
            _ty: PhantomData,
        }
    }
}

impl<T: ?Sized> From<Vec<u8>> for ResourceBorrow<T> {
    fn from(repr: Vec<u8>) -> Self {
        Self {
            repr: repr.into(),
            _ty: PhantomData,
        }
    }
}

impl<T: ?Sized> From<ResourceBorrow<T>> for Bytes {
    fn from(ResourceBorrow { repr, .. }: ResourceBorrow<T>) -> Self {
        repr
    }
}

impl<T: ?Sized> PartialEq for ResourceBorrow<T> {
    fn eq(&self, other: &Self) -> bool {
        self.repr == other.repr
    }
}

impl<T: ?Sized> Eq for ResourceBorrow<T> {}

impl<T: ?Sized> Hash for ResourceBorrow<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.repr.hash(state);
    }
}

impl<T: ?Sized> AsRef<[u8]> for ResourceBorrow<T> {
    fn as_ref(&self) -> &[u8] {
        &self.repr
    }
}

impl<T: ?Sized> AsRef<Bytes> for ResourceBorrow<T> {
    fn as_ref(&self) -> &Bytes {
        &self.repr
    }
}

impl<T: ?Sized + 'static> Debug for ResourceBorrow<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "borrow<{:?}>", TypeId::of::<T>())
    }
}

impl<T: ?Sized> ResourceBorrow<T> {
    pub fn new(repr: impl Into<Bytes>) -> Self {
        Self::from(repr.into())
    }
}

#[repr(transparent)]
pub struct ResourceOwn<T: ?Sized> {
    repr: Bytes,
    _ty: PhantomData<T>,
}

impl<T: ?Sized> From<ResourceOwn<T>> for ResourceBorrow<T> {
    fn from(ResourceOwn { repr, _ty }: ResourceOwn<T>) -> Self {
        Self {
            repr,
            _ty: PhantomData,
        }
    }
}

impl<T: ?Sized> From<Bytes> for ResourceOwn<T> {
    fn from(repr: Bytes) -> Self {
        Self {
            repr,
            _ty: PhantomData,
        }
    }
}

impl<T: ?Sized> From<Vec<u8>> for ResourceOwn<T> {
    fn from(repr: Vec<u8>) -> Self {
        Self {
            repr: repr.into(),
            _ty: PhantomData,
        }
    }
}

impl<T: ?Sized> From<ResourceOwn<T>> for Bytes {
    fn from(ResourceOwn { repr, .. }: ResourceOwn<T>) -> Self {
        repr
    }
}

impl<T: ?Sized> PartialEq for ResourceOwn<T> {
    fn eq(&self, other: &Self) -> bool {
        self.repr == other.repr
    }
}

impl<T: ?Sized> Eq for ResourceOwn<T> {}

impl<T: ?Sized> Hash for ResourceOwn<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.repr.hash(state);
    }
}

impl<T: ?Sized> AsRef<[u8]> for ResourceOwn<T> {
    fn as_ref(&self) -> &[u8] {
        &self.repr
    }
}

impl<T: ?Sized> AsRef<Bytes> for ResourceOwn<T> {
    fn as_ref(&self) -> &Bytes {
        &self.repr
    }
}

impl<T: ?Sized + 'static> Debug for ResourceOwn<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "own<{:?}>", TypeId::of::<T>())
    }
}

impl<T: ?Sized> ResourceOwn<T> {
    pub fn new(repr: impl Into<Bytes>) -> Self {
        Self::from(repr.into())
    }

    pub fn as_borrow(&self) -> ResourceBorrow<T> {
        ResourceBorrow {
            repr: self.repr.clone(),
            _ty: PhantomData,
        }
    }
}

pub type DeferredFn<T> = Box<
    dyn FnOnce(Arc<T>, Vec<usize>) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>
        + Send,
>;

pub trait Deferred<T> {
    fn take_deferred(&mut self) -> Option<DeferredFn<T>>;
}

macro_rules! impl_deferred_sync {
    ($t:ty) => {
        impl<T> Deferred<T> for $t {
            fn take_deferred(&mut self) -> Option<DeferredFn<T>> {
                None
            }
        }
    };
}

impl_deferred_sync!(BoolCodec);
impl_deferred_sync!(S8Codec);
impl_deferred_sync!(U8Codec);
impl_deferred_sync!(S16Codec);
impl_deferred_sync!(U16Codec);
impl_deferred_sync!(S32Codec);
impl_deferred_sync!(U32Codec);
impl_deferred_sync!(S64Codec);
impl_deferred_sync!(U64Codec);
impl_deferred_sync!(F32Codec);
impl_deferred_sync!(F64Codec);
impl_deferred_sync!(CoreNameDecoder);
impl_deferred_sync!(CoreNameEncoder);
impl_deferred_sync!(CoreVecDecoderBytes);
impl_deferred_sync!(CoreVecEncoderBytes);
impl_deferred_sync!(Utf8Codec);
impl_deferred_sync!(PrimValEncoder);
impl_deferred_sync!(Leb128Encoder);
impl_deferred_sync!(Leb128DecoderI8);
impl_deferred_sync!(Leb128DecoderU8);
impl_deferred_sync!(Leb128DecoderI16);
impl_deferred_sync!(Leb128DecoderU16);
impl_deferred_sync!(Leb128DecoderI32);
impl_deferred_sync!(Leb128DecoderU32);
impl_deferred_sync!(Leb128DecoderI64);
impl_deferred_sync!(Leb128DecoderU64);
impl_deferred_sync!(Leb128DecoderI128);
impl_deferred_sync!(Leb128DecoderU128);
impl_deferred_sync!(ResourceEncoder);
impl_deferred_sync!(UnitCodec);
impl_deferred_sync!(ListDecoderU8);

impl_deferred_sync!(CoreVecDecoder<BoolCodec>);
impl_deferred_sync!(CoreVecDecoder<S8Codec>);
impl_deferred_sync!(CoreVecDecoder<U8Codec>);
impl_deferred_sync!(CoreVecDecoder<S16Codec>);
impl_deferred_sync!(CoreVecDecoder<U16Codec>);
impl_deferred_sync!(CoreVecDecoder<S32Codec>);
impl_deferred_sync!(CoreVecDecoder<U32Codec>);
impl_deferred_sync!(CoreVecDecoder<S64Codec>);
impl_deferred_sync!(CoreVecDecoder<U64Codec>);
impl_deferred_sync!(CoreVecDecoder<F32Codec>);
impl_deferred_sync!(CoreVecDecoder<F64Codec>);
impl_deferred_sync!(CoreVecDecoder<CoreNameDecoder>);
impl_deferred_sync!(CoreVecDecoder<CoreVecDecoderBytes>);
impl_deferred_sync!(CoreVecDecoder<Utf8Codec>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderI8>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderU8>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderI16>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderU16>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderI32>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderU32>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderI64>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderU64>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderI128>);
impl_deferred_sync!(CoreVecDecoder<Leb128DecoderU128>);
impl_deferred_sync!(CoreVecDecoder<UnitCodec>);

pub struct SyncCodec<T>(pub T);

impl<T> Deref for SyncCodec<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for SyncCodec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T, C> Deferred<T> for SyncCodec<C> {
    fn take_deferred(&mut self) -> Option<DeferredFn<T>> {
        None
    }
}

impl<T: Default> Default for SyncCodec<T> {
    fn default() -> Self {
        Self(T::default())
    }
}

impl<T, I> tokio_util::codec::Encoder<I> for SyncCodec<T>
where
    T: tokio_util::codec::Encoder<I>,
{
    type Error = T::Error;

    fn encode(&mut self, item: I, dst: &mut BytesMut) -> Result<(), Self::Error> {
        self.0.encode(item, dst)
    }
}

impl<T> tokio_util::codec::Decoder for SyncCodec<T>
where
    T: tokio_util::codec::Decoder,
{
    type Item = T::Item;
    type Error = T::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.0.decode(src)
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.0.decode_eof(buf)
    }

    fn framed<IO: AsyncRead + AsyncWrite + Sized>(
        self,
        io: IO,
    ) -> tokio_util::codec::Framed<IO, Self>
    where
        Self: Sized,
    {
        self.0.framed(io).map_codec(Self)
    }
}


pub async fn handle_deferred<T, I>(
    w: Arc<T>,
    deferred: I,
    mut path: Vec<usize>,
    idx: u64,
) -> std::io::Result<()>
where
    I: IntoIterator<Item = Option<DeferredFn<T>>>,
    I::IntoIter: ExactSizeIterator,
{
    let mut futs = FuturesUnordered::default();
    for (i, f) in zip(0.., deferred) {
        if let Some(f) = f {
            path.push(i);
            futs.push(f(Arc::clone(&w), path.clone()));
            path.pop();
        }
    }
    while let Some(()) = futs.try_next().await? {}
    Ok(())
}

pub trait Encode<T>: Sized {
    type Encoder: tokio_util::codec::Encoder<Self> + Deferred<T> + Default + Send;


    fn encode(
        self,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error>
    {
        enc.encode(self, dst)?;
        Ok(enc.take_deferred())
    }


    fn encode_iter_own<I>(
        items: I,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
        idx: u64,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error>
    where
        I: IntoIterator<Item = Self>,
        I::IntoIter: ExactSizeIterator,
        T: crate::Index<T> + Send + Sync + 'static,
    {
        let items = items.into_iter();
        dst.reserve(items.len());
        let mut deferred = Vec::with_capacity(items.len());
        for item in items {
            enc.encode(item, dst)?;
            deferred.push(enc.take_deferred());
        }
        if deferred.iter().any(Option::is_some) {
            Ok(Some(Box::new(move |w, path| {
                Box::pin(handle_deferred(w, deferred, path, idx))
            })))
        } else {
            Ok(None)
        }
    }


    fn encode_iter_ref<'a, I>(
        items: I,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
        idx: u64,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<&'a Self>>::Error>
    where
        I: IntoIterator<Item = &'a Self>,
        I::IntoIter: ExactSizeIterator,
        T: crate::Index<T> + Send + Sync + 'static,
        Self::Encoder: tokio_util::codec::Encoder<&'a Self>,
    {
        let items = items.into_iter();
        dst.reserve(items.len());
        let mut deferred = Vec::with_capacity(items.len());
        for item in items {
            enc.encode(item, dst)?;
            deferred.push(enc.take_deferred());
        }
        if deferred.iter().any(Option::is_some) {
            Ok(Some(Box::new(move |w, path| {
                Box::pin(handle_deferred(w, deferred, path, idx))
            })))
        } else {
            Ok(None)
        }
    }


    fn encode_list_own(
        items: Vec<Self>,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error>
    where
        T: crate::Index<T> + Send + Sync + 'static,
    {
        let n = u32::try_from(items.len())
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        dst.reserve(5 + items.len());
        Leb128Encoder.encode(n, dst)?;
        Self::encode_iter_own(items, enc, dst, 0)
    }


    fn encode_list_ref<'a>(
        items: &'a [Self],
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<&'a Self>>::Error>
    where
        T: crate::Index<T> + Send + Sync + 'static,
        Self::Encoder: tokio_util::codec::Encoder<&'a Self>,
    {
        let n = u32::try_from(items.len())
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        dst.reserve(5 + items.len());
        Leb128Encoder.encode(n, dst)?;
        Self::encode_iter_ref(items, enc, dst, 0)
    }
}

pub trait Decode<T>: Sized {
    type Decoder: tokio_util::codec::Decoder<Item = Self> + Deferred<T> + Default + Send + 'static;
    type ListDecoder: tokio_util::codec::Decoder<Item = Vec<Self>> + Default + 'static;
}

impl<T, W> Deferred<W> for OptionEncoder<T>
where
    T: Deferred<W>,
{
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        self.0.take_deferred()
    }
}

impl<T, W> Encode<W> for Option<T>
where
    T: Encode<W>,
{
    type Encoder = OptionEncoder<T::Encoder>;
}

impl<'a, T, W> Encode<W> for &'a Option<T>
where
    T: Encode<W>,
    T::Encoder: tokio_util::codec::Encoder<&'a T>,
{
    type Encoder = OptionEncoder<T::Encoder>;
}

impl<T, W> Deferred<W> for OptionDecoder<T>
where
    T: Deferred<W> + Default,
{
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        mem::take(self).into_inner().take_deferred()
    }
}

impl<T, R> Decode<R> for Option<T>
where
    T: Decode<R>,
{
    type Decoder = OptionDecoder<T::Decoder>;
    type ListDecoder = CoreVecDecoder<Self::Decoder>;
}

impl<O, E, W> Deferred<W> for ResultEncoder<O, E>
where
    O: Deferred<W>,
    E: Deferred<W>,
{
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        match (self.ok.take_deferred(), self.err.take_deferred()) {
            (None, None) => None,
            (Some(ok), None) => Some(ok),
            (None, Some(err)) => Some(err),
            (Some(ok), Some(_)) => {
                if cfg!(debug_assertions) {
                    panic!("both `result::ok` and `result::err` deferred function set")
                }
                Some(ok)
            }
        }
    }
}

impl<O, E, W> Encode<W> for Result<O, E>
where
    O: Encode<W>,
    E: Encode<W>,
    std::io::Error: From<<O::Encoder as tokio_util::codec::Encoder<O>>::Error>,
    std::io::Error: From<<E::Encoder as tokio_util::codec::Encoder<E>>::Error>,
{
    type Encoder = ResultEncoder<O::Encoder, E::Encoder>;
}

impl<'a, O, E, W> Encode<W> for &'a Result<O, E>
where
    O: Encode<W>,
    O::Encoder: tokio_util::codec::Encoder<&'a O>,
    E: Encode<W>,
    E::Encoder: tokio_util::codec::Encoder<&'a E>,
    std::io::Error: From<<O::Encoder as tokio_util::codec::Encoder<&'a O>>::Error>,
    std::io::Error: From<<E::Encoder as tokio_util::codec::Encoder<&'a E>>::Error>,
{
    type Encoder = ResultEncoder<O::Encoder, E::Encoder>;
}

impl<O, E, W> Deferred<W> for ResultDecoder<O, E>
where
    O: Deferred<W> + Default,
    E: Deferred<W> + Default,
{
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        let (mut ok, mut err) = mem::take(self).into_inner();
        match (ok.take_deferred(), err.take_deferred()) {
            (None, None) => None,
            (Some(ok), None) => Some(ok),
            (None, Some(err)) => Some(err),
            (Some(ok), Some(_)) => {
                if cfg!(debug_assertions) {
                    panic!("both `result::ok` and `result::err` deferred function set")
                }
                Some(ok)
            }
        }
    }
}

impl<O, E, R> Decode<R> for Result<O, E>
where
    O: Decode<R>,
    E: Decode<R>,
    std::io::Error: From<<O::Decoder as tokio_util::codec::Decoder>::Error>,
    std::io::Error: From<<E::Decoder as tokio_util::codec::Decoder>::Error>,
{
    type Decoder = ResultDecoder<O::Decoder, E::Decoder>;
    type ListDecoder = CoreVecDecoder<Self::Decoder>;
}

pub struct ListEncoder<W> {
    deferred: Option<DeferredFn<W>>,
}

impl<W> Default for ListEncoder<W> {
    fn default() -> Self {
        Self { deferred: None }
    }
}

impl<W> Deferred<W> for ListEncoder<W> {
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        self.deferred.take()
    }
}

impl<T, W> tokio_util::codec::Encoder<Vec<T>> for ListEncoder<W>
where
    T: Encode<W>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Error = <T::Encoder as tokio_util::codec::Encoder<T>>::Error;

    fn encode(&mut self, items: Vec<T>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut enc = T::Encoder::default();
        self.deferred = T::encode_list_own(items, &mut enc, dst)?;
        Ok(())
    }
}

impl<'a, T, W> tokio_util::codec::Encoder<&'a Vec<T>> for ListEncoder<W>
where
    T: Encode<W>,
    T::Encoder: tokio_util::codec::Encoder<&'a T>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Error = <T::Encoder as tokio_util::codec::Encoder<&'a T>>::Error;

    fn encode(&mut self, items: &'a Vec<T>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut enc = T::Encoder::default();
        self.deferred = T::encode_list_ref(items, &mut enc, dst)?;
        Ok(())
    }
}

impl<'a, 'b, T, W> tokio_util::codec::Encoder<&'a &'b Vec<T>> for ListEncoder<W>
where
    T: Encode<W>,
    T::Encoder: tokio_util::codec::Encoder<&'b T>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Error = <T::Encoder as tokio_util::codec::Encoder<&'b T>>::Error;

    fn encode(&mut self, items: &'a &'b Vec<T>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut enc = T::Encoder::default();
        self.deferred = T::encode_list_ref(items, &mut enc, dst)?;
        Ok(())
    }
}

impl<'a, T, W> tokio_util::codec::Encoder<&'a [T]> for ListEncoder<W>
where
    T: Encode<W>,
    T::Encoder: tokio_util::codec::Encoder<&'a T>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Error = <T::Encoder as tokio_util::codec::Encoder<&'a T>>::Error;

    fn encode(&mut self, items: &'a [T], dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut enc = T::Encoder::default();
        self.deferred = T::encode_list_ref(items, &mut enc, dst)?;
        Ok(())
    }
}

impl<'a, 'b, T, W> tokio_util::codec::Encoder<&'a &'b [T]> for ListEncoder<W>
where
    T: Encode<W>,
    T::Encoder: tokio_util::codec::Encoder<&'b T>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Error = <T::Encoder as tokio_util::codec::Encoder<&'b T>>::Error;

    fn encode(&mut self, items: &'a &'b [T], dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut enc = T::Encoder::default();
        self.deferred = T::encode_list_ref(items, &mut enc, dst)?;
        Ok(())
    }
}

impl<T, W> Encode<W> for Vec<T>
where
    T: Encode<W>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Encoder = ListEncoder<W>;
}

impl<'a, T, W> Encode<W> for &'a Vec<T>
where
    T: Encode<W>,
    T::Encoder: tokio_util::codec::Encoder<&'a T>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Encoder = ListEncoder<W>;
}

impl<'a, T, W> Encode<W> for &'a [T]
where
    T: Encode<W>,
    T::Encoder: tokio_util::codec::Encoder<&'a T>,
    W: crate::Index<W> + Send + Sync + 'static,
{
    type Encoder = ListEncoder<W>;
}

pub struct ListDecoder<T, R>
where
    T: tokio_util::codec::Decoder,
{
    dec: T,
    ret: Vec<T::Item>,
    cap: usize,
    deferred: Vec<Option<DeferredFn<R>>>,
}

impl<T, R> ListDecoder<T, R>
where
    T: tokio_util::codec::Decoder,
{
    pub fn new(dec: T) -> Self {
        Self {
            dec,
            ret: Vec::default(),
            cap: 0,
            deferred: vec![],
        }
    }
}

impl<T, R> Default for ListDecoder<T, R>
where
    T: tokio_util::codec::Decoder + Default,
{
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T, R> Deferred<R> for ListDecoder<T, R>
where
    T: tokio_util::codec::Decoder,
    R: crate::Index<R> + Send + Sync + 'static,
{
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        let deferred = mem::take(&mut self.deferred);
        if deferred.iter().any(Option::is_some) {
            Some(Box::new(|r, path| {
                Box::pin(handle_deferred(r, deferred, path, 0))
            }))
        } else {
            None
        }
    }
}

impl<T, R> tokio_util::codec::Decoder for ListDecoder<T, R>
where
    T: tokio_util::codec::Decoder + Deferred<R>,
{
    type Item = Vec<T::Item>;
    type Error = T::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if self.cap == 0 {
            let Some(len) = Leb128DecoderU32.decode(src)? else {
                return Ok(None);
            };
            if len == 0 {
                return Ok(Some(Vec::default()));
            }
            let len = len
                .try_into()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            self.ret = Vec::with_capacity(len);
            self.deferred = Vec::with_capacity(len);
            self.cap = len;
        }
        while self.cap > 0 {
            let Some(v) = self.dec.decode(src)? else {
                return Ok(None);
            };
            self.ret.push(v);
            self.deferred.push(self.dec.take_deferred());
            self.cap -= 1;
        }
        Ok(Some(mem::take(&mut self.ret)))
    }
}

impl<T, R> Decode<R> for Vec<T>
where
    T: Decode<R> + Send,
    T::ListDecoder: Deferred<R> + Send,
    R: crate::Index<R> + Send + 'static,
{
    type Decoder = T::ListDecoder;
    type ListDecoder = ListDecoder<Self::Decoder, R>;
}

macro_rules! impl_copy_codec {
    ($t:ty, $c:tt) => {
        impl<W> Encode<W> for $t {
            type Encoder = $c;


            fn encode_iter_own<I>(
                items: I,
                enc: &mut Self::Encoder,
                dst: &mut BytesMut,
                _idx: u64,
            ) -> Result<
                Option<DeferredFn<W>>,
                <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error,
            >
            where
                I: IntoIterator<Item = Self>,
                I::IntoIter: ExactSizeIterator,
            {
                let items = items.into_iter();
                dst.reserve(items.len());
                for item in items {
                    enc.encode(item, dst)?;
                }
                Ok(None)
            }


            fn encode_iter_ref<'a, I>(
                items: I,
                enc: &mut Self::Encoder,
                dst: &mut BytesMut,
                _idx: u64,
            ) -> Result<
                Option<DeferredFn<W>>,
                <Self::Encoder as tokio_util::codec::Encoder<&'a Self>>::Error,
            >
            where
                I: IntoIterator<Item = &'a Self>,
                I::IntoIter: ExactSizeIterator,
            {
                let items = items.into_iter();
                dst.reserve(items.len());
                for item in items {
                    enc.encode(*item, dst)?;
                }
                Ok(None)
            }
        }

        impl<'b, W> Encode<W> for &'b $t {
            type Encoder = $c;


            fn encode_iter_own<I>(
                items: I,
                enc: &mut Self::Encoder,
                dst: &mut BytesMut,
                _idx: u64,
            ) -> Result<
                Option<DeferredFn<W>>,
                <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error,
            >
            where
                I: IntoIterator<Item = Self>,
                I::IntoIter: ExactSizeIterator,
            {
                let items = items.into_iter();
                dst.reserve(items.len());
                for item in items {
                    enc.encode(*item, dst)?;
                }
                Ok(None)
            }


            fn encode_iter_ref<'a, I>(
                items: I,
                enc: &mut Self::Encoder,
                dst: &mut BytesMut,
                _idx: u64,
            ) -> Result<
                Option<DeferredFn<W>>,
                <Self::Encoder as tokio_util::codec::Encoder<&'a Self>>::Error,
            >
            where
                I: IntoIterator<Item = &'a Self>,
                I::IntoIter: ExactSizeIterator,
                'b: 'a,
            {
                let items = items.into_iter();
                dst.reserve(items.len());
                for item in items {
                    enc.encode(item, dst)?;
                }
                Ok(None)
            }
        }

        impl<R> Decode<R> for $t {
            type Decoder = $c;
            type ListDecoder = CoreVecDecoder<Self::Decoder>;
        }
    };
}

impl_copy_codec!(bool, BoolCodec);
impl_copy_codec!(i8, S8Codec);
impl_copy_codec!(i16, S16Codec);
impl_copy_codec!(u16, U16Codec);
impl_copy_codec!(i32, S32Codec);
impl_copy_codec!(u32, U32Codec);
impl_copy_codec!(i64, S64Codec);
impl_copy_codec!(u64, U64Codec);
impl_copy_codec!(f32, F32Codec);
impl_copy_codec!(f64, F64Codec);
impl_copy_codec!(char, Utf8Codec);

impl<T> Encode<T> for u8 {
    type Encoder = U8Codec;


    fn encode_iter_own<I>(
        items: I,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
        _idx: u64,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error>
    where
        I: IntoIterator<Item = Self>,
        I::IntoIter: ExactSizeIterator,
    {
        let items = items.into_iter();
        dst.reserve(items.len());
        dst.extend(items);
        Ok(None)
    }


    fn encode_iter_ref<'a, I>(
        items: I,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
        _idx: u64,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<&'a Self>>::Error>
    where
        I: IntoIterator<Item = &'a Self>,
        I::IntoIter: ExactSizeIterator,
    {
        let items = items.into_iter();
        dst.reserve(items.len());
        dst.extend(items);
        Ok(None)
    }


    fn encode_list_own(
        items: Vec<Self>,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error>
    {
        CoreVecEncoderBytes.encode(items, dst)?;
        Ok(None)
    }


    fn encode_list_ref<'a>(
        items: &'a [Self],
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<&'a Self>>::Error>
    where
        Self::Encoder: tokio_util::codec::Encoder<&'a Self>,
    {
        CoreVecEncoderBytes.encode(items, dst)?;
        Ok(None)
    }
}

impl<'b, T> Encode<T> for &'b u8 {
    type Encoder = U8Codec;


    fn encode_iter_own<I>(
        items: I,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
        _idx: u64,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<Self>>::Error>
    where
        I: IntoIterator<Item = Self>,
        I::IntoIter: ExactSizeIterator,
    {
        let items = items.into_iter();
        dst.reserve(items.len());
        dst.extend(items);
        Ok(None)
    }


    fn encode_iter_ref<'a, I>(
        items: I,
        enc: &mut Self::Encoder,
        dst: &mut BytesMut,
        _idx: u64,
    ) -> Result<Option<DeferredFn<T>>, <Self::Encoder as tokio_util::codec::Encoder<&'a Self>>::Error>
    where
        I: IntoIterator<Item = &'a Self>,
        I::IntoIter: ExactSizeIterator,
        'b: 'a,
    {
        let items = items.into_iter();
        dst.reserve(items.len());
        dst.extend(items.map(|b| **b));
        Ok(None)
    }
}

#[derive(Debug, Default)]
#[repr(transparent)]
pub struct ListDecoderU8(CoreVecDecoderBytes);

impl tokio_util::codec::Decoder for ListDecoderU8 {
    type Item = Vec<u8>;
    type Error = <CoreVecDecoderBytes as tokio_util::codec::Decoder>::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let Some(buf) = self.0.decode(src)? else {
            return Ok(None);
        };
        Ok(Some(buf.into()))
    }
}

impl<R> Decode<R> for u8 {
    type Decoder = U8Codec;
    type ListDecoder = ListDecoderU8;
}

impl<W> Encode<W> for &str {
    type Encoder = CoreNameEncoder;
}

impl<W> Encode<W> for &&str {
    type Encoder = CoreNameEncoder;
}

impl<W> Encode<W> for String {
    type Encoder = CoreNameEncoder;
}

impl<W> Encode<W> for &String {
    type Encoder = CoreNameEncoder;
}

impl<R> Decode<R> for String {
    type Decoder = CoreNameDecoder;
    type ListDecoder = CoreVecDecoder<Self::Decoder>;
}

impl<W> Encode<W> for Bytes {
    type Encoder = CoreVecEncoderBytes;
}

impl<W> Encode<W> for &Bytes {
    type Encoder = CoreVecEncoderBytes;
}

impl<R> Decode<R> for Bytes {
    type Decoder = CoreVecDecoderBytes;
    type ListDecoder = CoreVecDecoder<Self::Decoder>;
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct ResourceEncoder;

impl<T: ?Sized> tokio_util::codec::Encoder<ResourceOwn<T>> for ResourceEncoder {
    type Error = std::io::Error;


    fn encode(&mut self, item: ResourceOwn<T>, dst: &mut BytesMut) -> std::io::Result<()> {
        CoreVecEncoderBytes.encode(item.repr, dst)
    }
}

impl<T: ?Sized> tokio_util::codec::Encoder<&ResourceOwn<T>> for ResourceEncoder {
    type Error = std::io::Error;


    fn encode(&mut self, item: &ResourceOwn<T>, dst: &mut BytesMut) -> std::io::Result<()> {
        CoreVecEncoderBytes.encode(&item.repr, dst)
    }
}

impl<T: ?Sized, W> Encode<W> for ResourceOwn<T> {
    type Encoder = ResourceEncoder;
}

impl<T: ?Sized, W> Encode<W> for &ResourceOwn<T> {
    type Encoder = ResourceEncoder;
}

impl<T: ?Sized> tokio_util::codec::Encoder<ResourceBorrow<T>> for ResourceEncoder {
    type Error = std::io::Error;


    fn encode(&mut self, item: ResourceBorrow<T>, dst: &mut BytesMut) -> std::io::Result<()> {
        CoreVecEncoderBytes.encode(item.repr, dst)
    }
}

impl<T: ?Sized> tokio_util::codec::Encoder<&ResourceBorrow<T>> for ResourceEncoder {
    type Error = std::io::Error;


    fn encode(&mut self, item: &ResourceBorrow<T>, dst: &mut BytesMut) -> std::io::Result<()> {
        CoreVecEncoderBytes.encode(&item.repr, dst)
    }
}

impl<T: ?Sized, W> Encode<W> for ResourceBorrow<T> {
    type Encoder = ResourceEncoder;
}

impl<T: ?Sized, W> Encode<W> for &ResourceBorrow<T> {
    type Encoder = ResourceEncoder;
}

#[derive(Debug)]
#[repr(transparent)]
pub struct ResourceBorrowDecoder<T: ?Sized> {
    dec: CoreVecDecoderBytes,
    _ty: PhantomData<T>,
}

impl<T: ?Sized> Default for ResourceBorrowDecoder<T> {
    fn default() -> Self {
        Self {
            dec: CoreVecDecoderBytes::default(),
            _ty: PhantomData,
        }
    }
}

impl<R, T: ?Sized> Deferred<R> for ResourceBorrowDecoder<T> {
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        None
    }
}

impl<R, T: ?Sized> Deferred<R> for CoreVecDecoder<ResourceBorrowDecoder<T>> {
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        None
    }
}

impl<R, T: ?Sized + Send + 'static> Decode<R> for ResourceBorrow<T> {
    type Decoder = ResourceBorrowDecoder<T>;
    type ListDecoder = CoreVecDecoder<Self::Decoder>;
}

impl<T: ?Sized> tokio_util::codec::Decoder for ResourceBorrowDecoder<T> {
    type Item = ResourceBorrow<T>;
    type Error = std::io::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let repr = self.dec.decode(src)?;
        Ok(repr.map(Self::Item::from))
    }
}

#[derive(Debug)]
#[repr(transparent)]
pub struct ResourceOwnDecoder<T: ?Sized> {
    dec: CoreVecDecoderBytes,
    _ty: PhantomData<T>,
}

impl<T: ?Sized> Default for ResourceOwnDecoder<T> {
    fn default() -> Self {
        Self {
            dec: CoreVecDecoderBytes::default(),
            _ty: PhantomData,
        }
    }
}

impl<R, T: ?Sized> Deferred<R> for ResourceOwnDecoder<T> {
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        None
    }
}

impl<R, T: ?Sized> Deferred<R> for CoreVecDecoder<ResourceOwnDecoder<T>> {
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        None
    }
}

impl<R, T: ?Sized + Send + 'static> Decode<R> for ResourceOwn<T> {
    type Decoder = ResourceOwnDecoder<T>;
    type ListDecoder = CoreVecDecoder<Self::Decoder>;
}

impl<T: ?Sized> tokio_util::codec::Decoder for ResourceOwnDecoder<T> {
    type Item = ResourceOwn<T>;
    type Error = std::io::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let repr = self.dec.decode(src)?;
        Ok(repr.map(Self::Item::from))
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct UnitCodec;

impl tokio_util::codec::Encoder<()> for UnitCodec {
    type Error = std::io::Error;


    fn encode(&mut self, (): (), dst: &mut BytesMut) -> std::io::Result<()> {
        Ok(())
    }
}

impl tokio_util::codec::Encoder<&()> for UnitCodec {
    type Error = std::io::Error;


    fn encode(&mut self, (): &(), dst: &mut BytesMut) -> std::io::Result<()> {
        Ok(())
    }
}

impl tokio_util::codec::Decoder for UnitCodec {
    type Item = ();
    type Error = std::io::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        Ok(Some(()))
    }
}

/// Marker trait for [Encode] tuple types
pub trait TupleEncode<W>: Encode<W> {}

/// Marker trait for [Decode] tuple types
pub trait TupleDecode<R>: Decode<R> {}

impl<W> Encode<W> for () {
    type Encoder = UnitCodec;
}

impl<W> TupleEncode<W> for () {}

impl<R> Decode<R> for () {
    type Decoder = UnitCodec;
    type ListDecoder = CoreVecDecoder<Self::Decoder>;
}

impl<R> TupleDecode<R> for () {}

macro_rules! impl_tuple_codec {
    ($($vn:ident),+; $($vt:ident),+; $($cn:ident),+; $($ct:ident),+) => {
        impl<W, $($ct),+> Deferred<W> for TupleEncoder::<($($ct),+,)>
        where
            W: crate::Index<W> + Send + Sync + 'static,
            $($ct: Deferred<W> + Default + 'static),+
        {
            fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
                let Self(($(mut $cn),+,)) = mem::take(self);
                let deferred = [ $($cn.take_deferred()),+ ];
                if deferred.iter().any(Option::is_some) {
                    Some(Box::new(|r, path| Box::pin(handle_deferred(r, deferred, path, 0))))
                } else {
                    None
                }
            }
        }

        impl<W, E, $($vt),+> Encode<W> for ($($vt),+,)
        where
            W: crate::Index<W> + Send + Sync + 'static,
            E: From<std::io::Error>,
            $(
                $vt: Encode<W>,
                $vt::Encoder: tokio_util::codec::Encoder<$vt, Error = E> + 'static,
            )+
        {
            type Encoder = TupleEncoder::<($($vt::Encoder),+,)>;
        }

        impl<W, E, $($vt),+> TupleEncode<W> for ($($vt),+,)
        where
            W: crate::Index<W> + Send + Sync + 'static,
            E: From<std::io::Error>,
            $(
                $vt: Encode<W>,
                $vt::Encoder: tokio_util::codec::Encoder<$vt, Error = E> + 'static,
            )+
        {
        }

        impl<'a, W, E, $($vt),+> Encode<W> for &'a ($($vt),+,)
        where
            W: crate::Index<W> + Send + Sync + 'static,
            E: From<std::io::Error>,
            $(
                $vt: Encode<W>,
                $vt::Encoder: tokio_util::codec::Encoder<&'a $vt, Error = E> + 'static,
            )+
        {
            type Encoder = TupleEncoder::<($($vt::Encoder),+,)>;
        }

        impl<R, $($vt),+> Deferred<R> for TupleDecoder::<($($vt::Decoder),+,), ($(Option<$vt>),+,)>
        where
            R: crate::Index<R> + Send + Sync + 'static,
            $($vt: Decode<R>),+
        {
            fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
                let ($(mut $cn),+,) = mem::take(self).into_inner();
                let deferred = [ $($cn.take_deferred()),+ ];
                if deferred.iter().any(Option::is_some) {
                    Some(Box::new(|r, path| Box::pin(handle_deferred(r, deferred, path, 0))))
                } else {
                    None
                }
            }
        }

        impl<R, E, $($vt),+> Decode<R> for ($($vt),+,)
        where
            R: crate::Index<R> + Send + Sync + 'static,
            E: From<std::io::Error>,
            $(
                $vt: Decode<R> + Send + 'static,
                $vt::Decoder: tokio_util::codec::Decoder<Error = E> + Send + 'static,
            )+
        {
            type Decoder = TupleDecoder::<($($vt::Decoder),+,), ($(Option<$vt>),+,)>;
            type ListDecoder = ListDecoder<Self::Decoder, R>;
        }

        impl<R, E, $($vt),+> TupleDecode<R> for ($($vt),+,)
        where
            R: crate::Index<R> + Send + Sync + 'static,
            E: From<std::io::Error>,
            $(
                $vt: Decode<R> + Send + 'static,
                $vt::Decoder: tokio_util::codec::Decoder<Error = E> + Send + 'static,
            )+
        {
        }
    };
}

impl_tuple_codec!(
    v0;
    V0;
    c0;
    C0
);

impl_tuple_codec!(
    v0, v1;
    V0, V1;
    c0, c1;
    C0, C1
);

impl_tuple_codec!(
    v0, v1, v2;
    V0, V1, V2;
    c0, c1, c2;
    C0, C1, C2
);

impl_tuple_codec!(
    v0, v1, v2, v3;
    V0, V1, V2, V3;
    c0, c1, c2, c3;
    C0, C1, C2, C3
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4;
    V0, V1, V2, V3, V4;
    c0, c1, c2, c3, c4;
    C0, C1, C2, C3, C4
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5;
    V0, V1, V2, V3, V4, V5;
    c0, c1, c2, c3, c4, c5;
    C0, C1, C2, C3, C4, C5
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6;
    V0, V1, V2, V3, V4, V5, V6;
    c0, c1, c2, c3, c4, c5, c6;
    C0, C1, C2, C3, C4, C5, C6
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7;
    V0, V1, V2, V3, V4, V5, V6, V7;
    c0, c1, c2, c3, c4, c5, c6, c7;
    C0, C1, C2, C3, C4, C5, C6, C7
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8;
    V0, V1, V2, V3, V4, V5, V6, V7, V8;
    c0, c1, c2, c3, c4, c5, c6, c7, c8;
    C0, C1, C2, C3, C4, C5, C6, C7, C8
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8, v9;
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9;
    c0, c1, c2, c3, c4, c5, c6, c7, c8, c9;
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10;
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10;
    c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10;
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11;
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11;
    c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11;
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12;
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12;
    c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12;
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13;
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13;
    c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13;
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14;
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14;
    c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14;
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14
);

impl_tuple_codec!(
    v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15;
    V0, V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15;
    c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15;
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14, C15
);

pub struct FutureEncoder<W> {
    deferred: Option<DeferredFn<W>>,
}

impl<W> Default for FutureEncoder<W> {
    fn default() -> Self {
        Self { deferred: None }
    }
}

impl<W> Deferred<W> for FutureEncoder<W> {
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        self.deferred.take()
    }
}

impl<T, W, Fut> tokio_util::codec::Encoder<Fut> for FutureEncoder<W>
where
    T: Encode<W>,
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
    Fut: Future<Output = T> + Send + 'static,
    std::io::Error: From<<T::Encoder as tokio_util::codec::Encoder<T>>::Error>,
{
    type Error = std::io::Error;


    fn encode(&mut self, item: Fut, dst: &mut BytesMut) -> std::io::Result<()> {
        // TODO: Check if future is resolved
        dst.reserve(1);
        dst.put_u8(0x00);
        self.deferred = Some(Box::new(|w, mut path| {
            Box::pin(async move {
                let mut root = w.index(&path).map_err(std::io::Error::other)?;
                let item = item.await;
                let mut enc = T::Encoder::default();
                let mut buf = BytesMut::default();
                enc.encode(item, &mut buf)?;
                try_join!(root.write_all(&buf), async {
                    if let Some(f) = enc.take_deferred() {
                        path.push(0);
                        f(w, path).await
                    } else {
                        Ok(())
                    }
                })?;
                Ok(())
            })
        }));
        Ok(())
    }
}

impl<T, W> Encode<W> for Pin<Box<dyn Future<Output = T> + Send>>
where
    T: Encode<W> + 'static,
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
    std::io::Error: From<<T::Encoder as tokio_util::codec::Encoder<T>>::Error>,
{
    type Encoder = FutureEncoder<W>;
}

pub struct FutureDecoder<T, R>
where
    T: Decode<R>,
{
    dec: OptionDecoder<T::Decoder>,
    deferred: Option<DeferredFn<R>>,
    _ty: PhantomData<T>,
}

impl<T, R> Default for FutureDecoder<T, R>
where
    T: Decode<R>,
{
    fn default() -> Self {
        Self {
            dec: OptionDecoder::default(),
            deferred: None,
            _ty: PhantomData,
        }
    }
}

impl<T, R> Deferred<R> for FutureDecoder<T, R>
where
    T: Decode<R>,
{
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        self.deferred.take()
    }
}

impl<T, R> tokio_util::codec::Decoder for FutureDecoder<T, R>
where
    T: Decode<R> + Send + 'static,
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
    std::io::Error: From<<T::Decoder as tokio_util::codec::Decoder>::Error>,
{
    type Item = Pin<Box<dyn Future<Output = T> + Send>>;
    type Error = <T::Decoder as tokio_util::codec::Decoder>::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let Some(item) = self.dec.decode(src)? else {
            return Ok(None);
        };
        if let Some(item) = item {
            self.deferred = self.dec.take_deferred();
            return Ok(Some(Box::pin(async { item })));
        }

        // future is pending
        let (tx, rx) = oneshot::channel();
        let dec = mem::take(&mut self.dec).into_inner();
        self.deferred = Some(Box::new(|r, mut path| {
            Box::pin(async move {
                let indexed = r.index(&path).map_err(std::io::Error::other)?;
                let mut dec = FramedRead::new(indexed, dec);
                trace!("receiving future element");
                let Some(item) = dec.next().await else {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                };
                let item = item?;
                try_join!(
                    async {
                        tx.send(item).map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "future receiver closed",
                            )
                        })
                    },
                    async {
                        if let Some(rx) = dec.decoder_mut().take_deferred() {
                            path.push(0);
                            rx(r, path).await
                        } else {
                            Ok(())
                        }
                    }
                )?;
                Ok(())
            })
        }));
        return Ok(Some(Box::pin(async {
            rx.await.expect("future I/O dropped")
        })));
    }
}

impl<T, R> Decode<R> for Pin<Box<dyn Future<Output = T> + Send>>
where
    T: Decode<R> + Send + 'static,
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
    std::io::Error: From<<T::Decoder as tokio_util::codec::Decoder>::Error>,
{
    type Decoder = FutureDecoder<T, R>;
    type ListDecoder = ListDecoder<Self::Decoder, R>;
}

pub struct StreamEncoder<W> {
    deferred: Option<DeferredFn<W>>,
}

impl<W> Default for StreamEncoder<W> {
    fn default() -> Self {
        Self { deferred: None }
    }
}

impl<W> Deferred<W> for StreamEncoder<W> {
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        self.deferred.take()
    }
}

impl<T, W, S> tokio_util::codec::Encoder<S> for StreamEncoder<W>
where
    T: Encode<W> + Send + 'static,
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
    S: Stream<Item = Vec<T>> + Send + Unpin + 'static,
    std::io::Error: From<<T::Encoder as tokio_util::codec::Encoder<T>>::Error>,
{
    type Error = std::io::Error;


    fn encode(&mut self, mut items: S, dst: &mut BytesMut) -> std::io::Result<()> {
        // TODO: Check if stream is resolved
        dst.reserve(1);
        dst.put_u8(0x00);
        self.deferred = Some(Box::new(|w, path| {
            Box::pin(async move {
                let mut root = w.index(&path).map_err(std::io::Error::other)?;
                let mut enc = T::Encoder::default();
                let mut buf = BytesMut::default();
                let mut tasks = JoinSet::new();
                let mut i = 0_u64;
                loop {
                    select! {
                        chunk = items.next() => {
                            let Some(chunk) = chunk else {
                                trace!("writing stream end");
                                buf.reserve(1);
                                buf.put_u8(0x00);
                                try_join!(
                                    root.write_all(&buf),
                                    async {
                                        while let Some(res) = tasks.join_next().await {
                                            trace!(?res, "receiver task finished");
                                            res??;
                                        }
                                    Ok(())
                                })?;
                                return Ok(())
                            };
                            let n = u32::try_from(chunk.len()).map_err(|err| {
                                std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
                            })?;
                            let end = i.checked_add(n.into()).ok_or_else(|| {
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidInput,
                                    "stream element index would overflow u64",
                                )
                            })?;
                            trace!(n, "encoding chunk length");
                            Leb128Encoder.encode(n, &mut buf)?;
                            trace!(i, buf = format!("{buf:02x?}"), "writing stream chunk items");
                            if let Some(deferred) = T::encode_iter_own(chunk, &mut enc, &mut buf, i)? {
                                trace!("spawning transmit task");
                                tasks.spawn(deferred(Arc::clone(&w), path.clone()));
                            }
                            i = end;
                        }
                        Some(res) = tasks.join_next() => {
                            trace!(?res, "receiver task finished");
                            res??;
                        }
                        res = root.write(&buf), if !buf.is_empty() => {
                            let n = res?;
                            trace!(?buf, n, "wrote bytes from buffer");
                            buf.advance(n);
                        }
                    }
                }
            })
        }));
        Ok(())
    }
}

impl<T, W> Encode<W> for Pin<Box<dyn Stream<Item = Vec<T>> + Send>>
where
    T: Encode<W> + Send + 'static,
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
    std::io::Error: From<<T::Encoder as tokio_util::codec::Encoder<T>>::Error>,
{
    type Encoder = StreamEncoder<W>;
}

pub struct StreamEncoderBytes<W> {
    deferred: Option<DeferredFn<W>>,
}

impl<W> Default for StreamEncoderBytes<W> {
    fn default() -> Self {
        Self { deferred: None }
    }
}

impl<W> Deferred<W> for StreamEncoderBytes<W> {
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        self.deferred.take()
    }
}

impl<W, S> tokio_util::codec::Encoder<S> for StreamEncoderBytes<W>
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
    S: Stream<Item = Bytes> + Send + Unpin + 'static,
{
    type Error = std::io::Error;


    fn encode(&mut self, mut items: S, dst: &mut BytesMut) -> std::io::Result<()> {
        // TODO: Check if reader is resolved
        dst.reserve(1);
        dst.put_u8(0x00);
        self.deferred = Some(Box::new(|w, path| {
            Box::pin(async move {
                let mut root = w.index(&path).map_err(std::io::Error::other)?;
                let mut buf = BytesMut::default();
                loop {
                    select! {
                        chunk = items.next() => {
                            let Some(chunk) = chunk else {
                                trace!("writing stream end");
                                buf.reserve(1);
                                buf.put_u8(0x00);
                                return root.write_all(&buf).await
                            };
                            let n = u32::try_from(chunk.len()).map_err(|err| {
                                std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
                            })?;
                            trace!(n, "encoding chunk length");
                            Leb128Encoder.encode(n, &mut buf)?;
                            buf.extend_from_slice(&chunk);
                        }
                        res = root.write(&buf), if !buf.is_empty() => {
                            let n = res?;
                            buf.advance(n);
                        }
                    }
                }
            })
        }));
        Ok(())
    }
}

impl<W> Encode<W> for Pin<Box<dyn Stream<Item = Bytes> + Send>>
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderBytes<W>;
}

pub struct StreamEncoderRead<W> {
    deferred: Option<DeferredFn<W>>,
}

impl<W> Default for StreamEncoderRead<W> {
    fn default() -> Self {
        Self { deferred: None }
    }
}

impl<W> Deferred<W> for StreamEncoderRead<W> {
    fn take_deferred(&mut self) -> Option<DeferredFn<W>> {
        self.deferred.take()
    }
}

impl<W, S> tokio_util::codec::Encoder<S> for StreamEncoderRead<W>
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
    S: AsyncRead + Send + Unpin + 'static,
{
    type Error = std::io::Error;


    fn encode(&mut self, mut items: S, dst: &mut BytesMut) -> std::io::Result<()> {
        // TODO: Check if reader is resolved
        dst.reserve(1);
        dst.put_u8(0x00);
        self.deferred = Some(Box::new(|w, path| {
            Box::pin(async move {
                let mut root = w.index(&path).map_err(std::io::Error::other)?;
                let mut buf = BytesMut::default();
                let mut chunk = BytesMut::default();
                loop {
                    select! {
                        res = items.read_buf(&mut chunk) => {
                            let n = res?;
                            if n == 0 {
                                trace!("writing stream end");
                                buf.reserve(1);
                                buf.put_u8(0x00);
                                return root.write_all(&buf).await
                            }
                            let n = u32::try_from(n).map_err(|err| {
                                std::io::Error::new(std::io::ErrorKind::InvalidInput, err)
                            })?;
                            trace!(n, "encoding chunk length");
                            Leb128Encoder.encode(n, &mut buf)?;
                            buf.extend_from_slice(&chunk);
                            chunk.clear();
                        }
                        res = root.write(&buf), if !buf.is_empty() => {
                            let n = res?;
                            buf.advance(n);
                        }
                    }
                }
            })
        }));
        Ok(())
    }
}

impl<W> Encode<W> for Pin<Box<dyn AsyncRead + Send>>
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

impl<T, W> Encode<W> for std::io::Cursor<T>
where
    T: AsRef<[u8]> + Send + Unpin + 'static,
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

impl<W> Encode<W> for tokio::io::Empty
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

#[cfg(feature = "io-std")]
impl<W> Encode<W> for tokio::io::Stdin
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

#[cfg(feature = "fs")]
impl<W> Encode<W> for tokio::fs::File
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

#[cfg(feature = "net")]
impl<W> Encode<W> for tokio::net::TcpStream
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

#[cfg(all(unix, feature = "net"))]
impl<W> Encode<W> for tokio::net::UnixStream
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

#[cfg(all(unix, feature = "net"))]
impl<W> Encode<W> for tokio::net::unix::pipe::Receiver
where
    W: AsyncWrite + crate::Index<W> + Send + Sync + Unpin + 'static,
{
    type Encoder = StreamEncoderRead<W>;
}

pub struct StreamDecoder<T, R>
where
    T: Decode<R>,
{
    dec: T::ListDecoder,
    deferred: Option<DeferredFn<R>>,
    _ty: PhantomData<T>,
}

impl<T, R> Default for StreamDecoder<T, R>
where
    T: Decode<R>,
{
    fn default() -> Self {
        Self {
            dec: T::ListDecoder::default(),
            deferred: None,
            _ty: PhantomData,
        }
    }
}

impl<T, R> Deferred<R> for StreamDecoder<T, R>
where
    T: Decode<R>,
{
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        self.deferred.take()
    }
}


async fn handle_deferred_stream<C, T, R>(
    dec: C,
    r: Arc<R>,
    mut path: Vec<usize>,
    tx: mpsc::Sender<Vec<T>>,
) -> std::io::Result<()>
where
    C: tokio_util::codec::Decoder<Item = T> + Deferred<R>,
    R: AsyncRead + crate::Index<R> + Send + Unpin + 'static,
    std::io::Error: From<C::Error>,
{
    let dec = ListDecoder::new(dec);
    let indexed = r.index(&path).map_err(std::io::Error::other)?;
    let mut framed = FramedRead::new(indexed, dec);
    let mut tasks = JoinSet::new();
    let mut i = 0_usize;
    loop {
        trace!("receiving stream chunk");
        select! {
            Some(chunk) = framed.next() => {
                let chunk = chunk?;
                if chunk.is_empty() {
                    trace!("received stream end");
                    while let Some(res) = tasks.join_next().await {
                        res??;
                    }
                    return Ok(())
                }
                let end = i.checked_add(chunk.len()).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "stream element index would overflow usize",
                    )
                })?;
                trace!(i, end, "received stream chunk");
                tx.send(chunk).await.map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stream receiver closed")
                })?;
                for (i, deferred) in zip(i.., mem::take(&mut framed.decoder_mut().deferred)) {
                    if let Some(deferred) = deferred {
                        trace!(i, "handling async read");
                        path.push(i);
                        let indexed = r.index(&path).map_err(std::io::Error::other)?;
                        trace!("spawning receive task");
                        tasks.spawn(deferred(indexed.into(), path.clone()));
                        path.pop();
                    }
                }
                i = end;
            },
            Some(res) = tasks.join_next() => {
                trace!(?res, "receiver task finished");
                res??;
            }
        }
    }
}

impl<T, R> tokio_util::codec::Decoder for StreamDecoder<T, R>
where
    T: Decode<R> + Send + 'static,
    T::ListDecoder: Deferred<R>,
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
    <T::Decoder as tokio_util::codec::Decoder>::Error: Send,
    std::io::Error: From<<T::Decoder as tokio_util::codec::Decoder>::Error>,
{
    type Item = Pin<Box<dyn Stream<Item = Vec<T>> + Send>>;
    type Error = <<T as Decode<R>>::ListDecoder as tokio_util::codec::Decoder>::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let Some(chunk) = self.dec.decode(src)? else {
            return Ok(None);
        };
        if !chunk.is_empty() {
            self.deferred = self.dec.take_deferred();
            return Ok(Some(Box::pin(stream::iter([chunk]))));
        }

        // stream is pending
        let (tx, rx) = mpsc::channel(128);
        self.deferred = Some(Box::new(|r, path| {
            Box::pin(
                async move { handle_deferred_stream(T::Decoder::default(), r, path, tx).await },
            )
        }));
        return Ok(Some(Box::pin(ReceiverStream::new(rx))));
    }
}

impl<T, R> Decode<R> for Pin<Box<dyn Stream<Item = Vec<T>> + Send>>
where
    T: Decode<R> + Send + 'static,
    T::ListDecoder: Deferred<R> + Send,
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
    <T::Decoder as tokio_util::codec::Decoder>::Error: Send,
    std::io::Error: From<<T::Decoder as tokio_util::codec::Decoder>::Error>,
{
    type Decoder = StreamDecoder<T, R>;
    type ListDecoder = ListDecoder<Self::Decoder, R>;
}

pub struct StreamDecoderBytes<R> {
    dec: CoreVecDecoderBytes,
    deferred: Option<DeferredFn<R>>,
}

impl<R> Default for StreamDecoderBytes<R> {
    fn default() -> Self {
        Self {
            dec: CoreVecDecoderBytes::default(),
            deferred: None,
        }
    }
}

impl<R> Deferred<R> for StreamDecoderBytes<R> {
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        self.deferred.take()
    }
}

impl<R> tokio_util::codec::Decoder for StreamDecoderBytes<R>
where
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
{
    type Item = Pin<Box<dyn Stream<Item = Bytes> + Send>>;
    type Error = std::io::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let Some(chunk) = self.dec.decode(src)? else {
            return Ok(None);
        };
        if !chunk.is_empty() {
            return Ok(Some(Box::pin(stream::iter([chunk]))));
        }

        // stream is pending
        let (tx, rx) = mpsc::channel(128);
        let dec = mem::take(&mut self.dec);
        self.deferred = Some(Box::new(|r, path| {
            Box::pin(async move {
                let indexed = r.index(&path).map_err(std::io::Error::other)?;
                let mut framed = FramedRead::new(indexed, dec);
                trace!("receiving stream chunk");
                while let Some(chunk) = framed.next().await {
                    let chunk = chunk?;
                    if chunk.is_empty() {
                        trace!("received stream end");
                        return Ok(());
                    }
                    trace!(?chunk, "received byte stream chunk");
                    tx.send(chunk).await.map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "stream receiver closed",
                        )
                    })?;
                }
                Ok(())
            })
        }));
        return Ok(Some(Box::pin(ReceiverStream::new(rx))));
    }
}

impl<R> Decode<R> for Pin<Box<dyn Stream<Item = Bytes> + Send>>
where
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
{
    type Decoder = StreamDecoderBytes<R>;
    type ListDecoder = ListDecoder<Self::Decoder, R>;
}

pub struct StreamDecoderRead<R> {
    dec: CoreVecDecoderBytes,
    deferred: Option<DeferredFn<R>>,
}

impl<R> Default for StreamDecoderRead<R> {
    fn default() -> Self {
        Self {
            dec: CoreVecDecoderBytes::default(),
            deferred: None,
        }
    }
}

impl<R> Deferred<R> for StreamDecoderRead<R> {
    fn take_deferred(&mut self) -> Option<DeferredFn<R>> {
        self.deferred.take()
    }
}

impl<R> tokio_util::codec::Decoder for StreamDecoderRead<R>
where
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
{
    type Item = Pin<Box<dyn AsyncRead + Send>>;
    type Error = std::io::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let Some(chunk) = self.dec.decode(src)? else {
            return Ok(None);
        };
        if !chunk.is_empty() {
            return Ok(Some(Box::pin(std::io::Cursor::new(chunk))));
        }

        // stream is pending
        let (tx, rx) = mpsc::channel(128);
        let dec = mem::take(&mut self.dec);
        self.deferred = Some(Box::new(|r, path| {
            Box::pin(async move {
                let indexed = r.index(&path).map_err(std::io::Error::other)?;
                let mut framed = FramedRead::new(indexed, dec);
                trace!("receiving stream chunk");
                while let Some(chunk) = framed.next().await {
                    let chunk = chunk?;
                    if chunk.is_empty() {
                        trace!("received stream end");
                        return Ok(());
                    }
                    trace!(?chunk, "received byte stream chunk");
                    tx.send(std::io::Result::Ok(chunk)).await.map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "stream receiver closed",
                        )
                    })?;
                }
                Ok(())
            })
        }));
        return Ok(Some(Box::pin(StreamReader::new(ReceiverStream::new(rx)))));
    }
}

impl<R> Decode<R> for Pin<Box<dyn AsyncRead + Send>>
where
    R: AsyncRead + crate::Index<R> + Send + Sync + Unpin + 'static,
{
    type Decoder = StreamDecoderRead<R>;
    type ListDecoder = ListDecoder<Self::Decoder, R>;
}

#[cfg(test)]
mod tests {
    use anyhow::bail;

    use super::*;

    struct NoopStream;

    impl crate::Index<Self> for NoopStream {
        fn index(&self, path: &[usize]) -> anyhow::Result<Self> {
            panic!("index should not be called with path {path:?}")
        }
    }

    #[test_log::test(tokio::test)]
    async fn codec() -> anyhow::Result<()> {
        let mut buf = BytesMut::new();
        let mut enc = <(u8, u32) as Encode<NoopStream>>::Encoder::default();
        enc.encode((0x42, 0x42), &mut buf)?;
        if let Some(_f) = Deferred::<NoopStream>::take_deferred(&mut enc) {
            bail!("no deferred write should have been returned");
        }
        assert_eq!(buf.as_ref(), b"\x42\x42");
        Ok(())
    }
}
