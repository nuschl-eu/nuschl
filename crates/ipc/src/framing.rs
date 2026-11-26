// SPDX-License-Identifier: EUPL-1.2

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::{Envelope, FRAME_VERSION, MAX_FRAME_BYTES};

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
        .map(decode_frame_body)
        .transpose()
}

pub async fn read_frame_async<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
    maximum: usize,
) -> Result<Envelope<T>, Error> {
    decode_frame_body(&read_body_async(reader, maximum).await?)
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
    write_body(writer, &encode_frame_body(value, maximum)?)
}

fn encode_frame_body<T: Serialize>(value: &Envelope<T>, maximum: usize) -> Result<Vec<u8>, Error> {
    let body = CappedBuffer {
        bytes: Vec::new(),
        maximum,
    };
    let body = postcard::to_io(value, body).map_err(|error| match error {
        postcard::Error::SerializeBufferFull => Error::TooLarge,
        error => Error::Postcard(error),
    })?;
    Ok(body.bytes)
}

pub(crate) fn decode_frame_body<T: DeserializeOwned>(body: &[u8]) -> Result<Envelope<T>, Error> {
    // The envelope begins with its version, then the request ID and payload.
    let (version, body) = postcard::take_from_bytes(body)?;
    if version != FRAME_VERSION {
        return Err(Error::Version(version));
    }
    let ((request_id, payload), remaining) = postcard::take_from_bytes(body)?;
    if !remaining.is_empty() {
        return Err(Error::TrailingBytes);
    }
    Ok(Envelope {
        version,
        request_id,
        payload,
    })
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
}

impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
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
        let mut body = encode_frame_body(&value, 16)?;
        assert_eq!(decode_frame_body::<u8>(&body)?, value);
        body.push(0);
        assert!(matches!(
            decode_frame_body::<u8>(&body),
            Err(Error::TrailingBytes)
        ));
        assert!(matches!(encode_frame_body(&value, 1), Err(Error::TooLarge)));
        Ok(())
    }
}
