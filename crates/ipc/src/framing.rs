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
    #[error("worker reply answers request {received}; expected {expected}")]
    WrongRequest { expected: u64, received: u64 },
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

/// Read the reply to request `expected` of at most `maximum` bytes. A reply
/// that answers another request is refused before its payload reaches the
/// caller.
pub async fn read_reply<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
    maximum: usize,
    expected: u64,
) -> Result<Envelope<T>, Error> {
    expect_request(read_frame_async(reader, maximum).await?, expected)
}

/// Read the reply to request `expected` from a frame body in memory, at most
/// `maximum` bytes long.
pub fn decode_reply<T: DeserializeOwned>(
    body: &[u8],
    maximum: usize,
    expected: u64,
) -> Result<Envelope<T>, Error> {
    if body.len() > maximum {
        return Err(Error::TooLarge);
    }
    expect_request(decode_frame_body(body)?, expected)
}

/// Read the reply to request `expected` of at most `maximum` bytes from a
/// blocking reader. End of input before the reply is an error.
pub fn read_reply_within<T: DeserializeOwned>(
    reader: &mut impl Read,
    maximum: usize,
    expected: u64,
) -> Result<Envelope<T>, Error> {
    let envelope = read_frame_within(reader, maximum)?.ok_or_else(|| {
        Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the worker closed its output",
        ))
    })?;
    expect_request(envelope, expected)
}

fn expect_request<T>(envelope: Envelope<T>, expected: u64) -> Result<Envelope<T>, Error> {
    if envelope.request_id != expected {
        return Err(Error::WrongRequest {
            expected,
            received: envelope.request_id,
        });
    }
    Ok(envelope)
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

    #[test]
    fn an_in_memory_reply_is_checked_for_its_request() -> Result<(), Error> {
        let body = encode_frame_body(&Envelope::new(4, 9_u8), MAX_FRAME_BYTES)?;
        assert_eq!(decode_reply::<u8>(&body, MAX_FRAME_BYTES, 4)?.payload, 9);
        assert!(matches!(
            decode_reply::<u8>(&body, MAX_FRAME_BYTES, 5),
            Err(Error::WrongRequest {
                expected: 5,
                received: 4
            })
        ));
        assert!(matches!(
            decode_reply::<u8>(&body, body.len() - 1, 4),
            Err(Error::TooLarge)
        ));
        Ok(())
    }

    #[test]
    fn a_blocking_reply_is_checked_for_its_request() -> Result<(), Error> {
        let mut body = Vec::new();
        write_frame(&mut body, &Envelope::new(4, 9_u8))?;
        let mut reader = body.as_slice();
        assert_eq!(
            read_reply_within::<u8>(&mut reader, MAX_FRAME_BYTES, 4)?.payload,
            9
        );
        let mut reader = body.as_slice();
        assert!(matches!(
            read_reply_within::<u8>(&mut reader, MAX_FRAME_BYTES, 5),
            Err(Error::WrongRequest { .. })
        ));
        // Nothing left to answer with.
        assert!(matches!(
            read_reply_within::<u8>(&mut [].as_slice(), MAX_FRAME_BYTES, 4),
            Err(Error::Io(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn an_async_reply_is_checked_before_it_is_unwrapped() -> Result<(), Error> {
        let mut body = Vec::new();
        write_frame(&mut body, &Envelope::new(2, "hi".to_owned()))?;
        let mut reader = body.as_slice();
        assert_eq!(
            read_reply::<String>(&mut reader, MAX_FRAME_BYTES, 2)
                .await?
                .payload,
            "hi"
        );
        let mut reader = body.as_slice();
        assert!(matches!(
            read_reply::<String>(&mut reader, MAX_FRAME_BYTES, 3).await,
            Err(Error::WrongRequest {
                expected: 3,
                received: 2
            })
        ));
        Ok(())
    }
}
