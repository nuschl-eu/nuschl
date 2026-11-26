// SPDX-License-Identifier: EUPL-1.2

use std::io::{self, Cursor, Read, Write};

use serde::{Deserialize as _, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::{Envelope, FRAME_VERSION, MAX_FRAME_BYTES};

const MAX_DEPTH: usize = 64;

#[derive(Debug, Error)]
pub enum Error {
    #[error("worker frame exceeds its size limit")]
    TooLarge,
    #[error("unsupported worker frame version {0}")]
    Version(u16),
    #[error("trailing bytes in a worker frame")]
    TrailingBytes,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Encode(#[from] rmp_serde::encode::Error),
    #[error(transparent)]
    Decode(#[from] rmp_serde::decode::Error),
    #[error(transparent)]
    Postcard(#[from] postcard::Error),
}

impl From<Error> for io::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Io(error) => error,
            error => io::Error::other(error),
        }
    }
}

pub fn read_frame<T: DeserializeOwned>(
    reader: &mut impl Read,
) -> Result<Option<Envelope<T>>, Error> {
    read_frame_within(reader, MAX_FRAME_BYTES)
}

/// Read a frame of at most `maximum` bytes. A clean end of input returns `None`.
pub fn read_frame_within<T: DeserializeOwned>(
    reader: &mut impl Read,
    maximum: usize,
) -> Result<Option<Envelope<T>>, Error> {
    read_body(reader, maximum)?
        .as_deref()
        .map(decode_messagepack)
        .transpose()
}

pub async fn read_frame_async<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
    maximum: usize,
) -> Result<Envelope<T>, Error> {
    decode_messagepack(&read_body_async(reader, maximum).await?)
}

pub fn write_frame<T: Serialize>(
    writer: &mut impl Write,
    value: &Envelope<T>,
) -> Result<(), Error> {
    write_frame_within(writer, value, MAX_FRAME_BYTES)
}

pub fn write_frame_within<T: Serialize>(
    writer: &mut impl Write,
    value: &Envelope<T>,
    maximum: usize,
) -> Result<(), Error> {
    let mut body = CappedBuffer::new(maximum);
    let result = value.serialize(
        &mut rmp_serde::Serializer::new(&mut body)
            .with_struct_map()
            .with_human_readable(),
    );
    write_body(writer, &body.finish(result)?)
}

pub fn read_postcard_frame<T: DeserializeOwned>(
    reader: &mut impl Read,
    maximum: usize,
) -> Result<Option<Envelope<T>>, Error> {
    read_body(reader, maximum)?
        .as_deref()
        .map(decode_postcard_body)
        .transpose()
}

pub async fn read_postcard_frame_async<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
    maximum: usize,
) -> Result<Envelope<T>, Error> {
    decode_postcard_body(&read_body_async(reader, maximum).await?)
}

pub fn write_postcard_frame<T: Serialize>(
    writer: &mut impl Write,
    value: &Envelope<T>,
    maximum: usize,
) -> Result<(), Error> {
    write_body(writer, &encode_postcard_body(value, maximum)?)
}

pub fn encode_postcard_body<T: Serialize>(
    value: &Envelope<T>,
    maximum: usize,
) -> Result<Vec<u8>, Error> {
    let mut body = CappedBuffer::new(maximum);
    let result = postcard::to_io(value, &mut body).map(|_| ());
    body.finish(result)
}

pub fn decode_postcard_body<T: DeserializeOwned>(body: &[u8]) -> Result<Envelope<T>, Error> {
    let (envelope, remaining) = postcard::take_from_bytes(body)?;
    checked(envelope, remaining.is_empty())
}

fn decode_messagepack<T: DeserializeOwned>(body: &[u8]) -> Result<Envelope<T>, Error> {
    let mut decoder = rmp_serde::Deserializer::new(Cursor::new(body)).with_human_readable();
    decoder.set_max_depth(MAX_DEPTH);
    let envelope = Envelope::deserialize(&mut decoder)?;
    checked(envelope, decoder.position() == body.len() as u64)
}

fn checked<T>(envelope: Envelope<T>, complete: bool) -> Result<Envelope<T>, Error> {
    if envelope.version != FRAME_VERSION {
        return Err(Error::Version(envelope.version));
    }
    if !complete {
        return Err(Error::TrailingBytes);
    }
    Ok(envelope)
}

fn read_body(reader: &mut impl Read, maximum: usize) -> Result<Option<Vec<u8>>, Error> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header[..1]) {
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        result => result?,
    }
    reader.read_exact(&mut header[1..])?;
    let mut body = allocate_body(u32::from_be_bytes(header), maximum)?;
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

async fn read_body_async(
    reader: &mut (impl AsyncRead + Unpin),
    maximum: usize,
) -> Result<Vec<u8>, Error> {
    let mut body = allocate_body(reader.read_u32().await?, maximum)?;
    reader.read_exact(&mut body).await?;
    Ok(body)
}

fn allocate_body(length: u32, maximum: usize) -> Result<Vec<u8>, Error> {
    let length = length as usize;
    if length > maximum {
        return Err(Error::TooLarge);
    }
    Ok(vec![0; length])
}

fn write_body(writer: &mut impl Write, body: &[u8]) -> Result<(), Error> {
    let length = u32::try_from(body.len()).map_err(|_| Error::TooLarge)?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

struct CappedBuffer {
    bytes: Vec<u8>,
    maximum: usize,
    full: bool,
}

impl CappedBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
            full: false,
        }
    }

    fn finish(self, result: Result<(), impl Into<Error>>) -> Result<Vec<u8>, Error> {
        if self.full {
            return Err(Error::TooLarge);
        }
        result.map_err(Into::into)?;
        Ok(self.bytes)
    }
}

impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            self.full = true;
            return Err(io::Error::other("worker frame too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postcard_frames_require_one_complete_supported_envelope() -> Result<(), Error> {
        let value = Envelope::new(7, 9_u8);
        let mut body = encode_postcard_body(&value, 16)?;
        assert_eq!(decode_postcard_body::<u8>(&body)?, value);
        body.push(0);
        assert!(matches!(
            decode_postcard_body::<u8>(&body),
            Err(Error::TrailingBytes)
        ));
        assert!(matches!(
            encode_postcard_body(&value, 1),
            Err(Error::TooLarge)
        ));
        Ok(())
    }

    #[test]
    fn messagepack_rejects_trailing_bytes() -> Result<(), Error> {
        let mut frame = Vec::new();
        write_frame(&mut frame, &Envelope::new(1, 3_u8))?;
        frame.push(0);
        assert!(matches!(
            decode_messagepack::<u8>(&frame[4..]),
            Err(Error::TrailingBytes)
        ));
        Ok(())
    }
}
