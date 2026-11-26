// SPDX-License-Identifier: EUPL-1.2
//! One response on a supervisor-approved transport. No URL, socket, hostname,
//! credentials or executable selection crosses this content-role protocol.
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::io;

use crate::{Envelope, decode_frame_body, read_frame_within, write_frame_within};

pub const SCHEMA: u16 = 1;
pub const MAX_CHUNK_BYTES: usize = 16 * 1024;
pub const MAX_REQUEST_BYTES: usize = MAX_CHUNK_BYTES + 32;
/// A whole document with the response fields.
pub const MAX_RESPONSE_BYTES: usize = crate::preview::MAX_DOCUMENT_BYTES + 64 * 1024;
pub const MAX_HEAD_BYTES: usize = 32 * 1024;
/// Lets a 10 MiB document arrive in reads averaging 1.3 KiB.
pub const MAX_CHUNKS: usize = 8192;
pub const MAX_INFORMATIONAL: usize = 4;
pub const RAW_OVERHEAD_BYTES: usize = 96 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Document,
    Image,
}
impl Purpose {
    pub fn body_limit(self) -> usize {
        match self {
            Self::Document => crate::preview::MAX_DOCUMENT_BYTES,
            Self::Image => 2 * 1024 * 1024,
        }
    }
    pub fn raw_limit(self) -> usize {
        self.body_limit() + RAW_OVERHEAD_BYTES
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Start {
        schema: u16,
        purpose: Purpose,
    },
    Chunk {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        eof: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Malformed,
    TooLarge,
    Unsupported,
    Unfinished,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    More,
    Complete(Response),
    Rejected(Failure),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub schema: u16,
    pub status: u16,
    pub location: Option<String>,
    pub media_type: Option<String>,
    pub charset: crate::preview::Charset,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
}
impl Response {
    pub fn validate(&self, purpose: Purpose) -> Result<(), Failure> {
        if self.schema != SCHEMA
            || !(200..=599).contains(&self.status)
            || self
                .location
                .as_ref()
                .is_some_and(|s| s.len() > 2048 || s.chars().any(char::is_control))
            || self
                .media_type
                .as_ref()
                .is_some_and(|s| s.len() > 128 || !s.is_ascii() || s.chars().any(char::is_control))
        {
            return Err(Failure::Malformed);
        }
        if self.body.len() > purpose.body_limit() {
            return Err(Failure::TooLarge);
        }
        Ok(())
    }
}

/// Encode a fetched body for the offline content worker's preview request.
pub fn encode_data(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Streamed transport input uses a request limit sized for one chunk.
pub fn read_request(reader: &mut impl io::Read) -> io::Result<Option<Envelope<Request>>> {
    Ok(read_frame_within(reader, MAX_REQUEST_BYTES)?)
}

pub fn encode_request(id: u64, request: &Request) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_frame_within(&mut bytes, &Envelope::new(id, request), MAX_REQUEST_BYTES)?;
    Ok(bytes)
}
pub fn write_reply(writer: &mut impl io::Write, id: u64, reply: &Reply) -> io::Result<()> {
    Ok(write_frame_within(
        writer,
        &Envelope::new(id, reply),
        MAX_RESPONSE_BYTES,
    )?)
}
pub fn decode_reply(id: u64, bytes: &[u8]) -> io::Result<Reply> {
    use io::{Error, ErrorKind};
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "preview parser reply frame limit",
        ));
    }
    let reply: Envelope<Reply> = decode_frame_body(bytes)?;
    if reply.request_id != id {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "preview parser reply identity",
        ));
    }
    Ok(reply.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn body_limits_and_response_scalars_are_checked() {
        let mut reply = Response {
            schema: SCHEMA,
            status: 200,
            location: None,
            media_type: Some("text/html".into()),
            charset: crate::preview::Charset::Utf8,
            body: b"page".to_vec(),
        };
        assert!(reply.validate(Purpose::Document).is_ok());
        reply.schema += 1;
        assert!(reply.validate(Purpose::Document).is_err());
        reply.schema = SCHEMA;
        reply.status = 101;
        assert!(reply.validate(Purpose::Document).is_err());
        reply.status = 200;
        reply.location = Some("https://example.org/\r\nx".into());
        assert!(reply.validate(Purpose::Document).is_err());
        reply.location = None;
        reply.body = vec![0; Purpose::Image.body_limit() + 1];
        assert_eq!(reply.validate(Purpose::Image), Err(Failure::TooLarge));
    }

    #[test]
    fn full_chunks_and_replies_travel_as_raw_bytes() {
        let bytes = vec![0xd3; MAX_CHUNK_BYTES];
        let request = Request::Chunk {
            data: bytes.clone(),
            eof: false,
        };
        let frame = encode_request(u64::MAX, &request).unwrap();
        assert!(frame.len() <= MAX_CHUNK_BYTES + 32);
        let request = read_request(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!(request.request_id, u64::MAX);
        assert!(matches!(request.payload, Request::Chunk { data, eof: false } if data == bytes));

        let reply = Reply::Complete(Response {
            schema: SCHEMA,
            status: 200,
            location: None,
            media_type: None,
            charset: crate::preview::Charset::Utf8,
            body: bytes.clone(),
        });
        let mut frame = Vec::new();
        write_reply(&mut frame, 7, &reply).unwrap();
        assert!(decode_reply(8, &frame[4..]).is_err());
        assert!(
            matches!(decode_reply(7, &frame[4..]).unwrap(), Reply::Complete(result) if result.body == bytes)
        );
    }
}
