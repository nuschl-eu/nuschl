// SPDX-License-Identifier: EUPL-1.2

#[cfg(target_os = "linux")]
mod capture;
#[cfg(any(target_os = "macos", windows))]
mod capture_native;
mod capture_pixels;
mod decode;
#[cfg(all(target_os = "linux", feature = "va-encoder"))]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "capture switches to the encoder core in the next step of #558"
    )
)]
mod va_encoder;

mod codec {
    #[cfg(target_os = "linux")]
    use nuschl_ipc::video::CaptureFailure;
    use nuschl_ipc::video::{HEADER_BYTES, Header, Kind, Settings};
    use rav1e::prelude::*;
    use std::io::{self, Read, Write};

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args = std::env::args_os().skip(1).collect::<Vec<_>>();
        match args.as_slice() {
            [encode] if encode == "--encode" => {
                encode_frames(&mut io::stdin().lock(), &mut io::stdout().lock())
            }
            #[cfg(target_os = "linux")]
            [encode, capture] if encode == "--encode" && capture == "--capture" => {
                if let Err(error) = super::capture::run() {
                    let reason = error
                        .downcast_ref::<CaptureFailure>()
                        .copied()
                        .unwrap_or(CaptureFailure::Software);
                    std::process::exit(i32::from(reason as u8));
                }
                Ok(())
            }
            [decode] if decode == "--decode" => super::decode::run(false),
            #[cfg(any(target_os = "macos", windows))]
            [source] if source == "--camera" || source == "--screen" => {
                use nuschl_ipc::video::Source;
                let source = if source == "--camera" {
                    Source::Camera
                } else {
                    Source::Screen
                };
                if let Err(reason) = super::capture_native::run(source) {
                    std::process::exit(i32::from(reason as u8));
                }
                Ok(())
            }
            #[cfg(target_os = "linux")]
            [decode] if decode == "--decode-vaapi" => super::decode::run(true),
            _ => Err("invalid video worker role".into()),
        }
    }

    fn encode_frames(
        input: &mut impl Read,
        output: &mut impl Write,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut header = [0; HEADER_BYTES];
        input.read_exact(&mut header)?;
        let setup = Header {
            bytes: Settings::BYTES as u32,
            ..Header::empty(Kind::Configure, 0)
        };
        if Header::from_bytes(header)? != setup {
            return Err("expected video encoder settings".into());
        }
        let mut settings = [0; Settings::BYTES];
        input.read_exact(&mut settings)?;
        let settings = Settings::from_bytes(settings)?;
        let frame_bytes = settings.frame_bytes()?;

        let config = config(settings);
        reply(output, Header::empty(Kind::Ready, 0), &[])?;
        let mut i420 = vec![0; frame_bytes];
        let mut previous = 0;
        while input.read(&mut header[..1])? != 0 {
            input.read_exact(&mut header[1..])?;
            let request = Header::from_bytes(header)?;
            if request.kind != Kind::Frame
                || request.id <= previous
                || request.bytes as usize != frame_bytes
            {
                return Err("invalid video encoder frame".into());
            }
            previous = request.id;
            input.read_exact(&mut i420)?;
            let packet = encode(&config, settings, &i420)?;
            if packet.len() > settings.packet_limit(true) {
                reply(output, Header::empty(Kind::Dropped, request.id), &[])?;
                continue;
            }
            let header = Header {
                kind: Kind::Encoded,
                key: true,
                id: request.id,
                bytes: u32::try_from(packet.len())?,
            };
            reply(output, header, &packet)?;
        }
        Ok(())
    }

    pub(super) fn config(settings: Settings) -> Config {
        let mut config = EncoderConfig::with_speed_preset(10);
        config.width = usize::from(settings.width);
        config.height = usize::from(settings.height);
        config.time_base = Rational::new(1, u64::from(settings.fps));
        config.low_latency = true;
        config.bitrate = i32::try_from(settings.bitrate).unwrap_or(i32::MAX);
        config.speed_settings.rdo_lookahead_frames = 1;
        Config::new().with_encoder_config(config).with_threads(1)
    }

    pub(super) fn encode(
        config: &Config,
        settings: Settings,
        i420: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut context = config.new_context::<u8>()?;
        let mut frame = context.new_frame();
        let width = usize::from(settings.width);
        let luma = width * usize::from(settings.height);
        let (y, uv) = i420.split_at(luma);
        let (u, v) = uv.split_at(luma / 4);
        frame.planes[0].copy_from_raw_u8(y, width, 1);
        frame.planes[1].copy_from_raw_u8(u, width / 2, 1);
        frame.planes[2].copy_from_raw_u8(v, width / 2, 1);

        // Flush the independent frame because rav1e's interframe lookahead
        // exceeds the live queue budget.
        context.send_frame(frame)?;
        context.flush();
        let packet = context.receive_packet()?;
        if packet.frame_type != FrameType::KEY || packet.data.is_empty() {
            return Err("expected an independent AV1 frame".into());
        }
        Ok(packet.data)
    }

    pub(super) fn reply(output: &mut impl Write, header: Header, bytes: &[u8]) -> io::Result<()> {
        output.write_all(&header.to_bytes())?;
        output.write_all(bytes)?;
        output.flush()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    nuschl_sandbox_windows::launch_if_requested();
    codec::run()
}
