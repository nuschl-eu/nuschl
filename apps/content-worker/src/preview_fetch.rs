// SPDX-License-Identifier: EUPL-1.2
//! HTTP/1 response parsing in the existing no-network content role. The trusted
//! supervisor already wrote its fixed GET to exactly one approved connection.
use nuschl_ipc::preview_fetch::{self as wire, Failure, Purpose, Reply, Request, Response};
use std::io;
use ureq_proto::{
    BodyMode,
    client::{
        Call, RecvResponseResult, SendRequestResult,
        state::{RecvBody, RecvResponse},
    },
};

pub fn run() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let first = wire::read_request(&mut input)?
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    if first.request_id != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "preview parser initial ID",
        ));
    }
    let purpose = match first.payload {
        Request::Start {
            schema: wire::SCHEMA,
            purpose,
        } => purpose,
        _ => {
            wire::write_reply(&mut output, 1, &Reply::Rejected(Failure::Malformed))?;
            return Ok(());
        }
    };
    let mut parser = Parser::new(purpose)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "preview parser configuration"))?;
    wire::write_reply(&mut output, 1, &Reply::More)?;
    let mut next = 2u64;
    loop {
        let request = wire::read_request(&mut input)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        if request.request_id != next {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "preview parser request ID",
            ));
        }
        next = next.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "preview parser ID exhaustion")
        })?;
        let result = match request.payload {
            Request::Chunk { data, eof } => parser.push(&data, eof),
            Request::Start { .. } => Err(Failure::Malformed), // Cannot reset aggregate budgets.
        };
        let reply = match result {
            Ok(Some(response)) => Reply::Complete(response),
            Ok(None) => Reply::More,
            Err(failure) => Reply::Rejected(failure),
        };
        let done = !matches!(reply, Reply::More);
        wire::write_reply(&mut output, request.request_id, &reply)?;
        if done {
            return Ok(());
        }
    }
}

enum Phase {
    Head(Call<RecvResponse>),
    Body(Call<RecvBody>),
}
struct Parser {
    purpose: Purpose,
    phase: Option<Phase>,
    pending: Vec<u8>,
    body: Vec<u8>,
    raw: usize,
    chunks: usize,
    head: usize,
    informational: usize,
    response: Option<Response>,
}
impl Parser {
    fn new(purpose: Purpose) -> Result<Self, Failure> {
        // This inert URI configures a maintained GET response state machine. It
        // is never opened, returned to the parent, or used as request authority.
        let request = ureq_proto::http::Request::get("http://parser.invalid/")
            .body(())
            .map_err(|_| Failure::Malformed)?;
        let mut send = Call::new(request)
            .map_err(|_| Failure::Malformed)?
            .proceed();
        let mut scratch = [0; 4096];
        send.write(&mut scratch).map_err(|_| Failure::Malformed)?;
        let Some(SendRequestResult::RecvResponse(head)) =
            send.proceed().map_err(|_| Failure::Malformed)?
        else {
            return Err(Failure::Malformed);
        };
        Ok(Self {
            purpose,
            phase: Some(Phase::Head(head)),
            pending: Vec::new(),
            body: Vec::new(),
            raw: 0,
            chunks: 0,
            head: 0,
            informational: 0,
            response: None,
        })
    }
    fn push(&mut self, bytes: &[u8], eof: bool) -> Result<Option<Response>, Failure> {
        if bytes.len() > wire::MAX_CHUNK_BYTES {
            return Err(Failure::TooLarge);
        }
        if bytes.is_empty() && !eof {
            return Err(Failure::Malformed);
        }
        self.chunks = self.chunks.checked_add(1).ok_or(Failure::TooLarge)?;
        self.raw = self.raw.checked_add(bytes.len()).ok_or(Failure::TooLarge)?;
        if self.chunks > wire::MAX_CHUNKS
            || self.raw > self.purpose.raw_limit()
            || self.pending.len() + bytes.len() > wire::MAX_HEAD_BYTES + wire::MAX_CHUNK_BYTES
        {
            return Err(Failure::TooLarge);
        }
        self.pending.extend_from_slice(bytes);
        loop {
            match self.phase.take().ok_or(Failure::Malformed)? {
                Phase::Head(mut call) => {
                    // Do not pass an oversized aggregate head to the library:
                    // pending may also contain body bytes following a valid head.
                    let remaining = wire::MAX_HEAD_BYTES
                        .checked_sub(self.head)
                        .ok_or(Failure::TooLarge)?;
                    let available = self.pending.len().min(remaining);
                    let (used, response) = call
                        .try_response(&self.pending[..available], false)
                        .map_err(|_| Failure::Malformed)?;
                    self.head = self.head.checked_add(used).ok_or(Failure::TooLarge)?;
                    self.pending.drain(..used);
                    let Some(response) = response else {
                        self.phase = Some(Phase::Head(call));
                        if used > 0 {
                            self.informational += 1;
                            if self.informational > wire::MAX_INFORMATIONAL {
                                return Err(Failure::TooLarge);
                            }
                            continue;
                        }
                        if self.head + self.pending.len() >= wire::MAX_HEAD_BYTES {
                            return Err(Failure::TooLarge);
                        }
                        return if eof {
                            Err(Failure::Unfinished)
                        } else {
                            Ok(None)
                        };
                    };
                    if !matches!(
                        response.version(),
                        ureq_proto::http::Version::HTTP_10 | ureq_proto::http::Version::HTTP_11
                    ) {
                        return Err(Failure::Unsupported);
                    }
                    let status = response.status().as_u16();
                    if status < 200 {
                        return Err(Failure::Unsupported);
                    } // Including protocol upgrades.
                    let headers = response.headers();
                    if headers.len() > 100 {
                        return Err(Failure::TooLarge);
                    }
                    let field = |name: &str, max: usize| -> Result<Option<String>, Failure> {
                        let mut values = headers.get_all(name).iter();
                        let value = values.next();
                        if values.next().is_some() {
                            return Err(Failure::Malformed);
                        }
                        value
                            .map(|v| {
                                v.to_str()
                                    .map(str::to_owned)
                                    .map_err(|_| Failure::Malformed)
                                    .and_then(|s| {
                                        if s.len() > max || s.chars().any(char::is_control) {
                                            Err(Failure::TooLarge)
                                        } else {
                                            Ok(s)
                                        }
                                    })
                            })
                            .transpose()
                    };
                    let encoding = field("content-encoding", 32)?;
                    if encoding
                        .as_ref()
                        .is_some_and(|s| !s.trim().eq_ignore_ascii_case("identity"))
                    {
                        return Err(Failure::Unsupported);
                    }
                    if headers.get_all("content-length").iter().count() > 1
                        || (headers.contains_key("content-length")
                            && headers.contains_key("transfer-encoding"))
                    {
                        return Err(Failure::Malformed);
                    }
                    let (media_type, charset) =
                        content_type(field("content-type", 256)?.as_deref())?;
                    self.response = Some(Response {
                        schema: wire::SCHEMA,
                        status,
                        location: field("location", 2048)?,
                        media_type,
                        charset,
                        body: Vec::new(),
                    });
                    if matches!(status, 301 | 302 | 303 | 307 | 308) {
                        if self
                            .response
                            .as_ref()
                            .and_then(|r| r.location.as_ref())
                            .is_none()
                        {
                            return Err(Failure::Malformed);
                        }
                        return self.complete(); // Parent closes this connection; never follow here.
                    }
                    match call.proceed().ok_or(Failure::Malformed)? {
                        RecvResponseResult::RecvBody(body) => {
                            if self.purpose == Purpose::Image
                                && let BodyMode::LengthDelimited(size) = body.body_mode()
                                && size > self.purpose.body_limit() as u64
                            {
                                return Err(Failure::TooLarge);
                            }
                            self.phase = Some(Phase::Body(body));
                        }
                        RecvResponseResult::Cleanup(_) | RecvResponseResult::Redirect(_) => {
                            if !self.pending.is_empty() {
                                return Err(Failure::Malformed);
                            }
                            return self.complete();
                        }
                    }
                }
                Phase::Body(mut call) => {
                    let mut out = [0; wire::MAX_CHUNK_BYTES];
                    let (used, written) = call
                        .read(&self.pending, &mut out)
                        .map_err(|_| Failure::Malformed)?;
                    let room = self.purpose.body_limit() - self.body.len();
                    if written > room {
                        // A document's metadata comes first: extract from what was read.
                        if self.purpose == Purpose::Image {
                            return Err(Failure::TooLarge);
                        }
                        self.body.extend_from_slice(&out[..room]);
                        return self.complete();
                    }
                    self.body.extend_from_slice(&out[..written]);
                    self.pending.drain(..used);
                    let complete = call.can_proceed()
                        && (!matches!(call.body_mode(), BodyMode::CloseDelimited) || eof);
                    self.phase = Some(Phase::Body(call));
                    if complete {
                        if !self.pending.is_empty() {
                            return Err(Failure::Malformed);
                        }
                        return self.complete();
                    }
                    if used > 0 || written > 0 {
                        continue;
                    }
                    if self.pending.len() > wire::MAX_HEAD_BYTES {
                        return Err(Failure::TooLarge);
                    }
                    return if eof {
                        Err(Failure::Unfinished)
                    } else {
                        Ok(None)
                    };
                }
            }
        }
    }
    fn complete(&mut self) -> Result<Option<Response>, Failure> {
        let mut response = self.response.take().ok_or(Failure::Malformed)?;
        response.body = std::mem::take(&mut self.body);
        response.validate(self.purpose)?;
        Ok(Some(response))
    }
}

/// Strict bounded header parameter classification stays in the isolated parser.
/// Unsupported encodings are reported as Other for offline extraction to refuse.
fn content_type(
    value: Option<&str>,
) -> Result<(Option<String>, nuschl_ipc::preview::Charset), Failure> {
    use nuschl_ipc::preview::Charset;
    let Some(value) = value else {
        return Ok((None, Charset::Unspecified));
    };
    let mut parts = value.split(';');
    let media = parts
        .next()
        .ok_or(Failure::Malformed)?
        .trim()
        .to_ascii_lowercase();
    if media.is_empty()
        || media.len() > 128
        || media.bytes().filter(|b| *b == b'/').count() != 1
        || !media
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/!#$&^_.+-".contains(&b))
    {
        return Err(Failure::Malformed);
    }
    let mut charset = None;
    let mut count = 0;
    for part in parts {
        count += 1;
        if count > 8 {
            return Err(Failure::TooLarge);
        }
        let (name, value) = part.trim().split_once('=').ok_or(Failure::Malformed)?;
        if name.trim().eq_ignore_ascii_case("charset") {
            if charset.is_some() {
                return Err(Failure::Malformed);
            }
            let value = value.trim();
            let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                &value[1..value.len() - 1]
            } else {
                value
            };
            if value.contains(['"', '\\']) || value.is_empty() {
                return Err(Failure::Malformed);
            }
            charset = Some(value);
        }
    }
    Ok((Some(media), Charset::from_label(charset)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(bytes: &[u8], purpose: Purpose, chunk: usize) -> Result<Response, Failure> {
        let mut parser = Parser::new(purpose)?;
        for part in bytes.chunks(chunk) {
            if let Some(reply) = parser.push(part, false)? {
                return Ok(reply);
            }
        }
        parser.push(&[], true)?.ok_or(Failure::Unfinished)
    }
    #[test]
    fn split_headers_fixed_chunked_close_delimited_and_no_content() {
        for width in [1, 7, 64, wire::MAX_CHUNK_BYTES] {
            for raw in [
                &b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Type: text/html\r\n\r\npage"[..],
                &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\npa\r\n2\r\nge\r\n0\r\n\r\n"[..],
                &b"HTTP/1.0 200 OK\r\n\r\npage"[..],
            ] {
                assert_eq!(parse(raw, Purpose::Document, width).unwrap().body, b"page");
            }
            assert_eq!(
                parse(b"HTTP/1.1 204 No Content\r\n\r\n", Purpose::Document, width)
                    .unwrap()
                    .body,
                b""
            );
        }
    }
    #[test]
    fn redirect_returned_without_fetch_and_compression_ambiguous_or_truncated_rejected() {
        let reply=parse(b"HTTP/1.1 302 Found\r\nLocation: https://example.org/next\r\nContent-Length: 99999999\r\n\r\n",Purpose::Document,16).unwrap();
        assert_eq!(reply.location.as_deref(), Some("https://example.org/next"));
        assert!(reply.body.is_empty());
        for raw in [
            &b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n\r\n"[..],
            &b"HTTP/1.1 101 Switching Protocols\r\n\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Length: 4\r\n\r\npage"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\npage"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\npage"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\npage\r\n0\r\n"[..],
        ] {
            assert!(parse(raw, Purpose::Document, 3).is_err(), "{raw:?}");
        }
    }
    #[test]
    fn a_long_document_ends_at_the_limit_and_a_long_image_is_rejected() {
        let limit = Purpose::Document.body_limit();
        let page = [b"<title>early</title>".as_slice(), &vec![b'x'; limit]].concat();
        let chunked: Vec<u8> = page
            .chunks(4096)
            .flat_map(|part| [format!("{:x}\r\n", part.len()).as_bytes(), part, b"\r\n"].concat())
            .collect();
        for raw in [
            [
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", page.len()).as_bytes(),
                &page,
            ]
            .concat(),
            [
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
                &chunked,
            ]
            .concat(),
            [b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &page].concat(),
        ] {
            let body = parse(&raw, Purpose::Document, wire::MAX_CHUNK_BYTES)
                .unwrap()
                .body;
            assert_eq!(body, page[..limit]);
        }
        let image = Purpose::Image.body_limit() + 1;
        let raw = [b"HTTP/1.0 200 OK\r\n\r\n".as_slice(), &vec![0; image]].concat();
        assert_eq!(
            parse(&raw, Purpose::Image, wire::MAX_CHUNK_BYTES).unwrap_err(),
            Failure::TooLarge
        );
    }
    /// The document's metadata from a chunked response read through `parse`.
    fn metadata(html: &[u8]) -> nuschl_ipc::preview::Response {
        let mut raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nX-Fill: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', 12 * 1024));
        raw.extend_from_slice(b"\r\nTransfer-Encoding: chunked\r\n\r\n");
        for part in html.chunks(8192) {
            raw.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            raw.extend_from_slice(part);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");
        let response = parse(&raw, Purpose::Document, wire::MAX_CHUNK_BYTES).unwrap();
        crate::preview::answer(nuschl_ipc::preview::Request::Document {
            schema: nuschl_ipc::preview::SCHEMA,
            body: wire::encode_data(&response.body),
            charset: response.charset,
            truncated: false,
        })
    }
    fn page(head: usize, body: usize) -> Vec<u8> {
        [
            b"<!DOCTYPE html><html><head><script>var c='".as_slice(),
            &vec![b'x'; head],
            b"';</script><meta property=\"og:title\" content=\"Synthetic video\"></head><body>",
            &vec![b'y'; body],
            b"</body></html>",
        ]
        .concat()
    }
    #[test]
    fn metadata_after_a_long_head_script_or_before_a_long_body_is_found() {
        use nuschl_ipc::preview::Response;
        // Shaped like a YouTube watch page: about 1.4 MiB, metadata after 760 KiB.
        // Then a page past the document limit with its metadata early.
        for html in [
            page(760 * 1024, 650 * 1024),
            page(0, Purpose::Document.body_limit()),
        ] {
            let Response::Metadata { metadata, .. } = metadata(&html) else {
                panic!("expected metadata");
            };
            assert_eq!(metadata.title, "Synthetic video");
        }
    }
    #[test]
    fn aggregate_head_body_informational_and_chunk_limits_do_not_reset() {
        assert!(
            parse(
                &[
                    b"HTTP/1.1 200 OK\r\nX: ".as_slice(),
                    &vec![b'x'; wire::MAX_HEAD_BYTES],
                    b"\r\n\r\n"
                ]
                .concat(),
                Purpose::Document,
                wire::MAX_CHUNK_BYTES
            )
            .is_err()
        );
        let huge = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            Purpose::Image.body_limit() + 1
        );
        assert!(parse(huge.as_bytes(), Purpose::Image, 32).is_err());
        let infos = [
            b"HTTP/1.1 103 Early Hints\r\n\r\n".repeat(wire::MAX_INFORMATIONAL + 1),
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
        ]
        .concat();
        assert!(parse(&infos, Purpose::Document, 64).is_err());
        let mut parser = Parser::new(Purpose::Document).unwrap();
        assert!(
            parser
                .push(b"HTTP/1.0 200 OK\r\n\r\n", false)
                .unwrap()
                .is_none()
        );
        for _ in 1..wire::MAX_CHUNKS {
            assert!(parser.push(b"x", false).unwrap().is_none());
        }
        assert!(parser.push(b"x", false).is_err());
    }
}
