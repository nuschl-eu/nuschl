// SPDX-License-Identifier: EUPL-1.2
//! The typed preview field of a Nuschl message: the carried value in
//! `nuschl_core::preview` converted to and from its wire message. Every value
//! is validated before it is written and after it is read, so a decoder never
//! copies more than the model's bounds and never yields an invalid preview.
use super::v1;
use nuschl_core::preview::{
    self, EmbeddedImage, ImageMime, MAX_ALT_BYTES, MAX_DESCRIPTION_BYTES, MAX_IMAGE_BYTES,
    MAX_TITLE_BYTES, MAX_URL_BYTES, Preview, ProviderId,
};

pub fn encode(preview: &Preview) -> Result<v1::Preview, preview::Error> {
    preview.validate()?;
    Ok(v1::Preview {
        version: u32::from(preview.version),
        provider: v1::preview::Provider::OpenGraphV1 as i32,
        url: preview.url.clone(),
        title: preview.title.clone(),
        description: preview.description.clone(),
        image: preview.image.as_ref().map(|image| v1::preview::Image {
            mime: match image.mime {
                ImageMime::Png => v1::preview::image::Mime::Png as i32,
                ImageMime::Jpeg => v1::preview::image::Mime::Jpeg as i32,
            },
            data: image.data.clone(),
            width: image.width,
            height: image.height,
            alt: image.alt.clone(),
        }),
    })
}

pub fn decode(wire: &v1::Preview) -> Result<Preview, preview::Error> {
    // Lengths first: nothing is copied before the model's bounds hold.
    if wire.url.len() > MAX_URL_BYTES {
        return Err(preview::Error::Url);
    }
    if wire.title.len() > MAX_TITLE_BYTES || wire.description.len() > MAX_DESCRIPTION_BYTES {
        return Err(preview::Error::Metadata);
    }
    let image = match &wire.image {
        Some(image) => Some(EmbeddedImage {
            mime: match v1::preview::image::Mime::try_from(image.mime) {
                Ok(v1::preview::image::Mime::Png) => ImageMime::Png,
                Ok(v1::preview::image::Mime::Jpeg) => ImageMime::Jpeg,
                Err(_) => return Err(preview::Error::Image),
            },
            data: image.data.clone(),
            width: image.width,
            height: image.height,
            alt: image.alt.clone(),
        }),
        None => None,
    };
    if image
        .as_ref()
        .is_some_and(|image| image.data.len() > MAX_IMAGE_BYTES || image.alt.len() > MAX_ALT_BYTES)
    {
        return Err(preview::Error::Image);
    }
    let preview = Preview {
        version: u16::try_from(wire.version).map_err(|_| preview::Error::Version)?,
        provider: match v1::preview::Provider::try_from(wire.provider) {
            Ok(v1::preview::Provider::OpenGraphV1) => ProviderId::OpenGraphV1,
            Err(_) => return Err(preview::Error::Version),
        },
        url: wire.url.clone(),
        title: wire.title.clone(),
        description: wire.description.clone(),
        image,
    };
    preview.validate()?;
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::super::MAX_PREVIEW_BYTES;
    use super::*;
    use prost::Message as _;

    fn preview() -> Preview {
        Preview {
            version: preview::VERSION,
            provider: ProviderId::OpenGraphV1,
            url: "https://example.org/page".into(),
            title: "A title".into(),
            description: "A description".into(),
            image: Some(EmbeddedImage {
                mime: ImageMime::Jpeg,
                data: vec![0xff; 1024],
                width: 640,
                height: 360,
                alt: "An alt".into(),
            }),
        }
    }

    #[test]
    fn a_carried_preview_round_trips_through_the_wire() {
        let carried = preview();
        let wire = encode(&carried).unwrap();
        assert_eq!(decode(&wire).unwrap(), carried);
        assert_eq!(wire.version, u32::from(preview::VERSION));
    }

    #[test]
    fn the_wire_covers_every_model_bound() {
        // The frame cap is derived from the model, so it must hold the largest
        // preview the model admits plus protobuf framing.
        let largest = Preview {
            version: preview::VERSION,
            provider: ProviderId::OpenGraphV1,
            url: format!("https://example.org/{}", "a".repeat(MAX_URL_BYTES - 20)),
            title: "t".repeat(MAX_TITLE_BYTES),
            description: "d".repeat(MAX_DESCRIPTION_BYTES),
            image: Some(EmbeddedImage {
                mime: ImageMime::Png,
                data: vec![0x7f; MAX_IMAGE_BYTES],
                width: preview::MAX_IMAGE_WIDTH,
                height: preview::MAX_IMAGE_HEIGHT,
                alt: "a".repeat(MAX_ALT_BYTES),
            }),
        };
        largest.validate().unwrap();
        let encoded = encode(&largest).unwrap().encode_to_vec();
        assert!(encoded.len() <= MAX_PREVIEW_BYTES, "{}", encoded.len());
        assert_eq!(
            decode(&prost::Message::decode(&*encoded).unwrap()).unwrap(),
            largest
        );
    }

    #[test]
    fn unreadable_previews_are_refused_before_they_are_copied() {
        let mut wire = encode(&preview()).unwrap();
        wire.version = 2;
        assert_eq!(decode(&wire), Err(preview::Error::Version));
        wire.version = u32::from(preview::VERSION);
        wire.provider = 7;
        assert_eq!(decode(&wire), Err(preview::Error::Version));
        wire.provider = v1::preview::Provider::OpenGraphV1 as i32;
        wire.url = "ftp://example.org/".into();
        assert_eq!(decode(&wire), Err(preview::Error::Url));
        wire.url = "https://example.org/page".into();
        wire.image.as_mut().unwrap().mime = 9;
        assert_eq!(decode(&wire), Err(preview::Error::Image));
        wire.image.as_mut().unwrap().mime = v1::preview::image::Mime::Png as i32;
        wire.image.as_mut().unwrap().data = vec![0; MAX_IMAGE_BYTES + 1];
        assert_eq!(decode(&wire), Err(preview::Error::Image));
        wire.image = None;
        wire.title = "t".repeat(MAX_TITLE_BYTES + 1);
        assert_eq!(decode(&wire), Err(preview::Error::Metadata));
    }

    #[test]
    fn an_invalid_preview_is_never_written() {
        let mut carried = preview();
        carried.title.clear();
        carried.description.clear();
        carried.image = None;
        assert_eq!(encode(&carried), Err(preview::Error::Metadata));
    }
}
