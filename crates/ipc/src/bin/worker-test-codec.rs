// SPDX-License-Identifier: EUPL-1.2
//! Test fixture converter. Python owns and drives the actual worker processes.

use std::io::{self, BufRead, Read as _, Write as _};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use nuschl_ipc::{
    ContentRequest, ContentResponse, Envelope, LiveMediaEvent, LiveMediaRequest, MAX_FRAME_BYTES,
    MediaRequest, MediaResponse, StorageRequest, StorageResponse, TransferEvent, TransferRequest,
    call_audio, math, read_frame_within, write_frame_within,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
// The fixture request may contain an entire worker frame in base64 or typed JSON.
const MAX_LINE: u64 = MAX_FRAME_BYTES as u64 * 8;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    CallAudio,
    Content,
    LiveMedia,
    Math,
    Media,
    Storage,
    Transfer,
}

#[derive(Deserialize)]
struct Request {
    role: Role,
    request_id: u64,
    #[serde(flatten)]
    direction: Direction,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Direction {
    Encode { payload: Value },
    Decode { frame: String },
}

impl Request {
    fn convert(self) -> Result<Value> {
        match self.role {
            Role::CallAudio => {
                self.exchange::<call_audio::Request, call_audio::Event>(call_audio::MAX_FRAME)
            }
            Role::Content => self.exchange::<ContentRequest, ContentResponse>(MAX_FRAME_BYTES),
            Role::LiveMedia => self.exchange::<LiveMediaRequest, LiveMediaEvent>(MAX_FRAME_BYTES),
            Role::Math => self.exchange::<math::MathRequest, math::MathResponse>(MAX_FRAME_BYTES),
            Role::Media => self.exchange::<MediaRequest, MediaResponse>(MAX_FRAME_BYTES),
            Role::Storage => self.exchange::<StorageRequest, StorageResponse>(MAX_FRAME_BYTES),
            Role::Transfer => self.exchange::<TransferRequest, TransferEvent>(MAX_FRAME_BYTES),
        }
    }

    fn exchange<ToWorker, FromWorker>(self, maximum: usize) -> Result<Value>
    where
        ToWorker: DeserializeOwned + Serialize,
        FromWorker: DeserializeOwned + Serialize,
    {
        match self.direction {
            Direction::Encode { payload } => {
                let payload: ToWorker = serde_json::from_value(payload)?;
                let mut frame = Vec::new();
                write_frame_within(
                    &mut frame,
                    &Envelope::new(self.request_id, payload),
                    maximum,
                )?;
                Ok(Value::String(STANDARD.encode(frame)))
            }
            Direction::Decode { frame } => {
                let bytes = STANDARD.decode(frame)?;
                let mut input = bytes.as_slice();
                let reply = read_frame_within::<FromWorker>(&mut input, maximum)?
                    .ok_or("missing worker reply")?;
                if reply.request_id != self.request_id || !input.is_empty() {
                    return Err("unexpected worker reply ID or trailing data".into());
                }
                Ok(serde_json::to_value(reply.payload)?)
            }
        }
    }
}

fn main() -> Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.by_ref().take(MAX_LINE).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.last() != Some(&b'\n') {
            return Err("incomplete or oversized test codec request".into());
        }
        let result = serde_json::from_slice::<Request>(&line)
            .map_err(Into::into)
            .and_then(Request::convert);
        let reply = match result {
            Ok(value) => json!({"value": value}),
            Err(error) => json!({"error": error.to_string()}),
        };
        serde_json::to_writer(&mut output, &reply)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}
