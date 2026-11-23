// SPDX-License-Identifier: EUPL-1.2
use std::io;

use base64::Engine as _;
use nuschl_content::ContentRenderer;
use nuschl_core::{ContentBlock, MessageDocument};
use nuschl_ipc::{
    ContentRequest, ContentResponse, EncodedImage, Envelope, MAX_FRAME_BYTES, PreparedContent,
    ThumbnailPng, read_frame_within, write_frame,
};

mod preview;
mod preview_fetch;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // On Windows, the launch stage starts this worker again in its AppContainer.
    #[cfg(windows)]
    nuschl_sandbox_windows::launch_if_requested();
    let mut arguments = std::env::args_os().skip(1);
    match (arguments.next(), arguments.next()) {
        (None, None) => {}
        (Some(role), None) if role == "--preview-fetch" => {
            return Ok(preview_fetch::run()?);
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid worker role").into()),
    }
    let renderer = ContentRenderer::new()?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    // Trusted requests: a preview document is the largest.
    let limit = MAX_FRAME_BYTES.max(nuschl_ipc::preview::MAX_REQUEST_FRAME_BYTES);
    while let Some(request) = read_frame_within::<ContentRequest>(&mut input, limit)? {
        let response = answer(&renderer, request.payload).unwrap_or_else(ContentResponse::Rejected);
        write_frame(&mut output, &Envelope::new(request.request_id, response))?;
    }
    Ok(())
}

fn answer(renderer: &ContentRenderer, request: ContentRequest) -> Result<ContentResponse, String> {
    let image = |block: Option<ContentBlock>| {
        block
            .map(|image| {
                ContentResponse::Rendered(MessageDocument {
                    blocks: vec![image],
                })
            })
            .ok_or_else(|| "unsupported or invalid image".to_owned())
    };
    let base64 = |text: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|_| "invalid base64".to_owned())
    };
    match request {
        ContentRequest::Preview(request) => Ok(ContentResponse::Preview(preview::answer(request))),
        ContentRequest::Render { text } => renderer
            .render(&text)
            .map(ContentResponse::Rendered)
            .map_err(|error| error.to_string()),
        ContentRequest::LegacyHtml { html } => renderer
            .legacy_plain_text(&html)
            .and_then(|source| {
                let document = renderer.render(&source)?;
                Ok(ContentResponse::Prepared(PreparedContent {
                    source,
                    document,
                }))
            })
            .map_err(|error| error.to_string()),
        ContentRequest::Avatar { image: encoded } => {
            image(nuschl_content::decode_image(&base64(&encoded)?, 96, 96))
        }
        ContentRequest::Thumbnail {
            image: encoded,
            max_width,
            max_height,
        } => {
            if max_width == 0
                || max_height == 0
                || max_width > nuschl_ipc::MAX_THUMBNAIL_EDGE
                || max_height > nuschl_ipc::MAX_THUMBNAIL_EDGE
            {
                return Err("thumbnail dimensions exceed their budget".to_owned());
            }
            if encoded.len() > nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES.div_ceil(3) * 4 {
                return Err("thumbnail input exceeds its budget".to_owned());
            }
            let bytes = base64(&encoded)?;
            if bytes.len() > nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES {
                return Err("thumbnail input exceeds its budget".to_owned());
            }
            let (width, height, png) = nuschl_content::thumbnail_png(&bytes, max_width, max_height)
                .ok_or_else(|| "unsupported or invalid image".to_owned())?;
            Ok(ContentResponse::Thumbnail(ThumbnailPng {
                width,
                height,
                png: base64::engine::general_purpose::STANDARD.encode(png),
            }))
        }
        ContentRequest::Icon { image: encoded } => {
            image(nuschl_content::decode_icon(&base64(&encoded)?))
        }
        ContentRequest::View { uri } => image(nuschl_content::decode_view(&uri)),
        ContentRequest::EncodeImage {
            image,
            limit,
            quality,
        } => nuschl_content::encode_image_message(&base64(&image)?, limit as usize, quality)
            .map(|(html, thumbnail)| ContentResponse::Encoded(EncodedImage { html, thumbnail }))
            .map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
mod thumbnail_tests {
    use super::*;
    #[test]
    fn thumbnail_bounds_invalid_input_and_bare_png() {
        let renderer = ContentRenderer::new().unwrap();
        let request = |image: &str, width, height| ContentRequest::Thumbnail {
            image: image.to_owned(),
            max_width: width,
            max_height: height,
        };
        let ContentResponse::Thumbnail(reply) = answer(&renderer, request("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC", 1, 1)).unwrap()
            else { panic!("expected a PNG"); };
        assert_eq!((reply.width, reply.height), (1, 1));
        let png = base64::engine::general_purpose::STANDARD
            .decode(&reply.png)
            .unwrap();
        // Only the chunks a browser needs, in the shape the bot checks.
        assert_eq!(nuschl_ipc::thumbnail::check_png(&png, 1, 1), Some((1, 1)));
        for (image, width, height) in [
            (
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC",
                0,
                1,
            ),
            (
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC",
                401,
                1,
            ),
            ("!!not-base64", 300, 300),
            ("", 300, 300),
            (
                "iVBORw0KGgoAAAANSUhEUgAAE4gAAAABCAIAAAC9c9PfAAAAJUlEQVR4nO3BgQAAAADDoPlTX+EAVQEAAAAAAAAAAAAAAAAAwGE6mQAB52yShgAAAABJRU5ErkJggg==",
                300,
                300,
            ),
        ] {
            assert!(answer(&renderer, request(image, width, height)).is_err());
        }
        let oversized = "A".repeat(nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES.div_ceil(3) * 4 + 1);
        assert!(answer(&renderer, request(&oversized, 300, 300)).is_err());
    }
}
