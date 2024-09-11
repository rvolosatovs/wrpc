use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use tracing::{instrument, trace};
use wasm_tokio::{Leb128DecoderU32, Leb128DecoderU64, Leb128Encoder};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub path: Arc<[usize]>,
    pub data: Bytes,
}

pub struct Decoder {
    path: Option<Vec<usize>>,
    path_cap: usize,
    data_len: usize,
    max_depth: u32,
    max_size: u64,
}

impl Decoder {
    #[must_use]
    pub fn new(max_depth: u32, max_size: u64) -> Self {
        Self {
            path: Option::default(),
            path_cap: 0,
            data_len: 0,
            max_depth,
            max_size,
        }
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new(32, u32::MAX.into())
    }
}

impl tokio_util::codec::Decoder for Decoder {
    type Item = Frame;
    type Error = std::io::Error;


    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let path = self.path.take();
        let mut path = if let Some(path) = path {
            path
        } else {
            trace!("decoding path length");
            let Some(n) = Leb128DecoderU32.decode(src)? else {
                return Ok(None);
            };
            trace!(n, "decoded path length");
            if n > self.max_depth {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "path length of `{n}` exceeds maximum of `{}`",
                        self.max_depth
                    ),
                ));
            }
            let n = n
                .try_into()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            self.path_cap = n;
            Vec::with_capacity(n)
        };
        let n = self.path_cap.saturating_sub(src.len());
        if n > 0 {
            src.reserve(n);
            self.path = Some(path);
            return Ok(None);
        }
        while self.path_cap > 0 {
            trace!(self.path_cap, "decoding path element");
            let Some(i) = Leb128DecoderU32.decode(src)? else {
                self.path = Some(path);
                return Ok(None);
            };
            trace!(i, "decoded path element");
            let i = i
                .try_into()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            path.push(i);
            self.path_cap -= 1;
        }
        if self.data_len == 0 {
            trace!("decoding data length");
            let Some(n) = Leb128DecoderU64.decode(src)? else {
                self.path = Some(path);
                return Ok(None);
            };
            trace!(n, "decoded data length");
            if n > self.max_size {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "payload length of `{n}` exceeds maximum of `{}`",
                        self.max_size
                    ),
                ));
            }
            let n = n
                .try_into()
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            self.data_len = n;
            if n == 0 {
                return Ok(Some(Frame {
                    path: Arc::from(path),
                    data: Bytes::default(),
                }));
            }
        }
        let n = self.data_len.saturating_sub(src.len());
        if n > 0 {
            src.reserve(n);
            self.path = Some(path);
            return Ok(None);
        }
        trace!(self.data_len, "decoding data");
        let data = src.split_to(self.data_len).freeze();
        self.data_len = 0;
        Ok(Some(Frame {
            path: Arc::from(path),
            data,
        }))
    }
}

pub struct FrameRef<'a> {
    pub path: &'a [usize],
    pub data: &'a [u8],
}

impl<'a> From<&'a Frame> for FrameRef<'a> {
    fn from(Frame { path, data }: &'a Frame) -> Self {
        Self {
            path: path.as_ref(),
            data: data.as_ref(),
        }
    }
}

pub struct Encoder;

impl tokio_util::codec::Encoder<FrameRef<'_>> for Encoder {
    type Error = std::io::Error;


    fn encode(
        &mut self,
        FrameRef { path, data }: FrameRef<'_>,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        let size = data.len();
        let depth = path.len();
        dst.reserve(size.saturating_add(depth).saturating_add(5 + 10));
        let n = u32::try_from(depth)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        trace!(n, "encoding path length");
        Leb128Encoder.encode(n, dst)?;
        for p in path {
            let p = u32::try_from(*p)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
            trace!(p, "encoding path element");
            Leb128Encoder.encode(p, dst)?;
        }
        let n = u64::try_from(size)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        trace!(n, "encoding data length");
        Leb128Encoder.encode(n, dst)?;
        dst.extend_from_slice(data);
        Ok(())
    }
}

impl tokio_util::codec::Encoder<&Frame> for Encoder {
    type Error = std::io::Error;


    fn encode(&mut self, frame: &Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        self.encode(FrameRef::from(frame), dst)
    }
}

#[cfg(test)]
mod tests {
    use futures::{SinkExt as _, TryStreamExt as _};
    use tokio_util::codec::{FramedRead, FramedWrite};

    use super::*;

    #[test_log::test(tokio::test)]
    async fn codec() -> std::io::Result<()> {
        let mut tx = FramedWrite::new(vec![], Encoder);

        tx.send(&Frame {
            path: [0, 1, 2].into(),
            data: "test".into(),
        })
        .await?;

        tx.send(FrameRef {
            path: &[],
            data: b"",
        })
        .await?;

        tx.send(FrameRef {
            path: &[0x42],
            data: "\x7fƒ𐍈Ő".as_bytes(),
        })
        .await?;

        let tx = tx.into_inner();
        assert_eq!(
            tx,
            concat!(
                concat!("\x03", concat!("\0", "\x01", "\x02"), "\x04test"),
                concat!("\0", "\0"),
                concat!("\x01", concat!("\x42"), "\x09\x7fƒ𐍈Ő"),
            )
            .as_bytes()
        );

        let mut rx = FramedRead::new(tx.as_slice(), Decoder::default());

        let s = rx.try_next().await?;
        assert_eq!(
            s,
            Some(Frame {
                path: [0, 1, 2].into(),
                data: "test".into(),
            })
        );

        let s = rx.try_next().await?;
        assert_eq!(
            s,
            Some(Frame {
                path: [].into(),
                data: "".into(),
            })
        );

        let s = rx.try_next().await?;
        assert_eq!(
            s,
            Some(Frame {
                path: [0x42].into(),
                data: "\x7fƒ𐍈Ő".into(),
            })
        );

        let s = rx.try_next().await.expect("failed to get EOF");
        assert_eq!(s, None);

        Ok(())
    }
}
