// SPDX-License-Identifier: EUPL-1.2
//! Run this parser in the content worker, never on the UI or audio thread.

use std::cell::OnceCell;

#[cfg(feature = "images")]
use base64::Engine as _;
#[cfg(feature = "images")]
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder as _, ImageEncoder as _, ImageFormat,
    ImageReader, Limits, metadata::Orientation,
};
use nuschl_core::{CodeRun, ContentBlock, MAX_CONTENT_BYTES, MessageDocument};
#[cfg(feature = "images")]
use nuschl_core::{
    ImageQuality,
    preview::{ImageMime, img_element},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use thiserror::Error;

pub mod markdown;
mod markup;
pub mod math;
pub mod preview;
#[cfg(feature = "images")]
mod strip;
pub use markup::{MAX_LINKS, MAX_URL_BYTES, bare_urls};
#[cfg(feature = "images")]
pub use strip::strip_metadata;

#[derive(Debug, Error)]
pub enum Error {
    #[error("message exceeds the {MAX_CONTENT_BYTES}-byte content processing limit")]
    TooLarge,
    #[error("syntax highlighting failed: {0}")]
    Highlight(#[from] syntect::Error),
    #[error("the bundled highlighting theme is missing")]
    MissingTheme,
    #[error("the bundled syntax definitions are invalid")]
    InvalidSyntaxBundle,
    #[error("unsupported or invalid image")]
    InvalidImage,
    #[error("the image cannot be made small enough for this server's {0}-byte limit")]
    ImageTooLarge(usize),
}

/// Inline images are shown as thumbnails; the worker's reply must stay below
/// the IPC frame limit, so the total decoded size per document is capped.
pub const MAX_IMAGE_WIDTH: u32 = 360;
pub const MAX_IMAGE_HEIGHT: u32 = 270;
/// The image viewer's size limit (its RGBA reply must fit one worker frame).
pub const VIEW_WIDTH: u32 = 1600;
pub const VIEW_HEIGHT: u32 = 1000;
#[cfg(feature = "images")]
const MAX_IMAGES: usize = 8;
#[cfg(feature = "images")]
const MAX_DOCUMENT_PIXELS_BYTES: usize = 640 * 1024;
#[cfg(feature = "images")]
const MAX_SOURCE_DIMENSION: u32 = 4096;

/// Decode PNG/JPEG/GIF (first frame) with dimension and allocation limits and
/// return a thumbnail no larger than `max_width` × `max_height`.
#[cfg(feature = "images")]
pub fn decode_image(bytes: &[u8], max_width: u32, max_height: u32) -> Option<ContentBlock> {
    let image = decode(
        bytes,
        &[
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
        ],
        MAX_SOURCE_DIMENSION,
        64 * 1024 * 1024,
    )?;
    let thumbnail = shrink(image, max_width, max_height).to_rgba8();
    Some(ContentBlock::Image {
        width: thumbnail.width(),
        height: thumbnail.height(),
        rgba: base64::engine::general_purpose::STANDARD.encode(thumbnail.as_raw()),
        source: None,
    })
}

/// Decode artwork like [`decode_image`] and re-encode the shrunk pixels as an
/// 8-bit RGBA PNG without any metadata, for browsers to show directly.
/// Returns width, height and the PNG.
#[cfg(feature = "images")]
pub fn thumbnail_png(bytes: &[u8], max_width: u32, max_height: u32) -> Option<(u32, u32, Vec<u8>)> {
    let image = decode(
        bytes,
        &[
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
        ],
        MAX_SOURCE_DIMENSION,
        64 * 1024 * 1024,
    )?;
    let pixels = shrink(image, max_width, max_height).to_rgba8();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            pixels.as_raw(),
            pixels.width(),
            pixels.height(),
            ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some((pixels.width(), pixels.height(), png))
}

/// A server's website icon (favicon: often ICO), at most 64 × 64.
#[cfg(feature = "images")]
pub fn decode_icon(bytes: &[u8]) -> Option<ContentBlock> {
    let formats = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::Ico,
        ImageFormat::Bmp,
        ImageFormat::WebP,
    ];
    let image = decode(bytes, &formats, 1024, 16 * 1024 * 1024)?;
    let icon = shrink(image, 64, 64).to_rgba8();
    Some(ContentBlock::Image {
        width: icon.width(),
        height: icon.height(),
        rgba: base64::engine::general_purpose::STANDARD.encode(icon.as_raw()),
        source: None,
    })
}

/// Decode one of `formats` (guessed from the bytes) within the given limits.
#[cfg(feature = "images")]
fn decode(
    bytes: &[u8],
    formats: &[ImageFormat],
    max_dimension: u32,
    max_alloc: u64,
) -> Option<DynamicImage> {
    decode_oriented(bytes, formats, max_dimension, max_alloc).map(|(image, _)| image)
}

/// [`decode`], and the EXIF orientation, which is not applied.
#[cfg(feature = "images")]
fn decode_oriented(
    bytes: &[u8],
    formats: &[ImageFormat],
    max_dimension: u32,
    max_alloc: u64,
) -> Option<(DynamicImage, Orientation)> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if !reader
        .format()
        .is_some_and(|format| formats.contains(&format))
    {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits.clone());
    // As `ImageReader::decode`: the pixels count against the allocation limit.
    let mut decoder = reader.into_decoder().ok()?;
    limits.reserve(decoder.total_bytes()).ok()?;
    decoder.set_limits(limits).ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    // A GIF's screen may be 0 pixels high, and the decoder accepts it: an
    // image without pixels is no image.
    DynamicImage::from_decoder(decoder)
        .ok()
        .filter(|image| image.width() > 0 && image.height() > 0)
        .map(|image| (image, orientation))
}

/// Shrink only: `thumbnail` would also enlarge a small image.
#[cfg(feature = "images")]
fn shrink(image: DynamicImage, max_width: u32, max_height: u32) -> DynamicImage {
    if image.width() > max_width || image.height() > max_height {
        image.thumbnail(max_width.max(1), max_height.max(1))
    } else {
        image
    }
}

/// Pasted screenshots may span several monitors.
#[cfg(feature = "images")]
const MAX_PASTE_DIMENSION: u32 = 8192;
/// The composer shows an attached image at most this large (device pixels).
pub const ATTACHMENT_THUMBNAIL: u32 = 128;

/// Re-encode a pasted image as a Mumble image message (an `<img>` element of
/// at most `limit` bytes), as stock Mumble does (`Log::imageToImg`): upright
/// as its EXIF orientation says, within the `quality`'s size, as JPEG at the
/// highest of its qualities that fits, or as PNG when that is smaller. When
/// even its lowest quality does not fit, the image shrinks until it does.
/// Also returns a thumbnail of at most [`ATTACHMENT_THUMBNAIL`] pixels.
#[cfg(feature = "images")]
pub fn encode_image_message(
    bytes: &[u8],
    limit: usize,
    quality: ImageQuality,
) -> Result<(String, ContentBlock), Error> {
    let limit = limit.min(MAX_CONTENT_BYTES);
    // The largest size, and the JPEG qualities tried in turn. Original is
    // re-encoded only where it cannot go as its file: as High.
    let ((view_width, view_height), qualities) = match quality {
        ImageQuality::Low => ((800, 500), [70, 55, 40]),
        ImageQuality::High | ImageQuality::Original => ((VIEW_WIDTH, VIEW_HEIGHT), [85, 70, 55]),
    };
    let formats = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        // Windows hands screenshots over as device-independent bitmaps.
        ImageFormat::Bmp,
        // Sent re-encoded as JPEG or PNG, which every Mumble client shows.
        ImageFormat::WebP,
    ];
    let (image, orientation) =
        decode_oriented(bytes, &formats, MAX_PASTE_DIMENSION, 256 * 1024 * 1024)
            .ok_or(Error::InvalidImage)?;
    let mut image = visible(image);
    // Shrunk before it is turned upright, into the box turned with it.
    let (width, height) = match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (view_height, view_width),
        _ => (view_width, view_height),
    };
    if image.width() > width || image.height() > height {
        image = fit(image, width, height);
    }
    image.apply_orientation(orientation);
    let thumbnail =
        if image.width() > ATTACHMENT_THUMBNAIL || image.height() > ATTACHMENT_THUMBNAIL {
            fit(image.clone(), ATTACHMENT_THUMBNAIL, ATTACHMENT_THUMBNAIL)
        } else {
            image.clone()
        }
        .to_rgba8();
    let thumbnail = ContentBlock::Image {
        width: thumbnail.width(),
        height: thumbnail.height(),
        rgba: base64::engine::general_purpose::STANDARD.encode(thumbnail.as_raw()),
        source: None,
    };
    loop {
        let png = encode_png(&image)?;
        // JPEG has no transparency: blend it onto white.
        let opaque = flatten(&image);
        let mut smallest = png.len();
        for quality in qualities {
            let jpeg = encode_jpeg(&opaque, quality)?;
            let (mime, bytes) = if png.len() <= jpeg.len() {
                (ImageMime::Png, &png)
            } else {
                (ImageMime::Jpeg, &jpeg)
            };
            let element = img_element(mime, bytes);
            if element.len() <= limit {
                return Ok((element, thumbnail));
            }
            smallest = smallest.min(jpeg.len());
        }
        // Encoded size follows the pixel count: aim a little below the limit.
        // An element is about 1.44 times its image (base64, percent-encoding, lines).
        let scale = ((limit as f64 / 1.44) / smallest as f64).sqrt().min(0.85) * 0.95;
        let (width, height) = (
            (image.width() as f64 * scale) as u32,
            (image.height() as f64 * scale) as u32,
        );
        if width == 0 || height == 0 {
            return Err(Error::ImageTooLarge(limit));
        }
        image = fit(image, width, height);
    }
}

/// `image` with only the colour that can be seen: a transparent image goes
/// through premultiplied 8-bit alpha and back, so a fully transparent pixel
/// carries no colour and a nearly transparent one only what it shows.
#[cfg(feature = "images")]
fn visible(image: DynamicImage) -> DynamicImage {
    if !image.color().has_alpha() {
        return image;
    }
    let mut pixels = image.into_rgba8();
    premultiply(&mut pixels);
    unpremultiply(&mut pixels);
    pixels.into()
}

/// Resize to fit `width` × `height`, keeping the aspect ratio, filtering a
/// transparent image premultiplied, so transparent pixels lend visible ones
/// none of their colour.
#[cfg(feature = "images")]
fn fit(image: DynamicImage, width: u32, height: u32) -> DynamicImage {
    let filter = image::imageops::FilterType::Triangle;
    let DynamicImage::ImageRgba8(mut pixels) = image else {
        return image.resize(width, height, filter);
    };
    premultiply(&mut pixels);
    let mut resized = DynamicImage::ImageRgba8(pixels)
        .resize(width, height, filter)
        .into_rgba8();
    unpremultiply(&mut resized);
    resized.into()
}

#[cfg(feature = "images")]
fn premultiply(pixels: &mut image::RgbaImage) {
    for pixel in pixels.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

#[cfg(feature = "images")]
fn unpremultiply(pixels: &mut image::RgbaImage) {
    for pixel in pixels.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = match alpha {
                0 => 0,
                _ => ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8,
            };
        }
    }
}

#[cfg(feature = "images")]
fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    let (width, height) = (image.width(), image.height());
    if image.color().has_alpha() {
        let pixels = image.to_rgba8();
        encoder.write_image(pixels.as_raw(), width, height, ExtendedColorType::Rgba8)
    } else {
        let pixels = image.to_rgb8();
        encoder.write_image(pixels.as_raw(), width, height, ExtendedColorType::Rgb8)
    }
    .map_err(|_| Error::InvalidImage)?;
    Ok(bytes)
}

#[cfg(feature = "images")]
fn encode_jpeg(image: &image::RgbImage, quality: u8) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|_| Error::InvalidImage)?;
    Ok(bytes)
}

#[cfg(feature = "images")]
fn flatten(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
        let [red, green, blue, alpha] = source.0.map(u32::from);
        let blend = |channel: u32| ((channel * alpha + 255 * (255 - alpha)) / 255) as u8;
        target.0 = [blend(red), blend(green), blend(blue)];
    }
    rgb
}

/// Decode an inline `data:` image for the viewer, up to `VIEW_WIDTH` × `VIEW_HEIGHT`.
#[cfg(feature = "images")]
pub fn decode_view(uri: &str) -> Option<ContentBlock> {
    decode_image(&data_uri(uri)?, VIEW_WIDTH, VIEW_HEIGHT)
}

/// Parse `data:image/<type>;base64,<payload>`; nothing else is accepted.
#[cfg(feature = "images")]
fn data_uri(uri: &str) -> Option<Vec<u8>> {
    Some(nuschl_core::data_uri_image(uri)?.0)
}

/// Loading syntect's syntaxes and theme costs most of a worker's start-up, so
/// it happens on the first code block rather than for every message.
#[derive(Default)]
pub struct ContentRenderer {
    highlighter: OnceCell<Highlighter>,
}

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

// Trusted generated data only. Generation parses the pinned upstream YAML;
// normal workers load only this binary asset, never YAML or user-selected grammars.
fn content_syntaxes() -> Result<SyntaxSet, Error> {
    syntect::dumps::from_uncompressed_data(include_bytes!("../assets/default-nix.packdump"))
        .map_err(|_| Error::InvalidSyntaxBundle)
}

impl ContentRenderer {
    /// Cannot fail since the highlighter loads lazily; it keeps its signature.
    pub fn new() -> Result<Self, Error> {
        Ok(Self::default())
    }

    fn highlighter(&self) -> Result<&Highlighter, Error> {
        if let Some(highlighter) = self.highlighter.get() {
            return Ok(highlighter);
        }
        let theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .ok_or(Error::MissingTheme)?;
        let syntaxes = content_syntaxes()?;
        Ok(self
            .highlighter
            .get_or_init(|| Highlighter { syntaxes, theme }))
    }

    /// Message text is Markdown (docs/markdown-rendering-plan.md), parsed
    /// once with the formulas of docs/math.md: CommonMark's emphasis, code,
    /// links to allowed targets, headings, lists, quotes, rules and code
    /// blocks, and `~~strikethrough~~`, with their syntax hidden. Every line
    /// break and blank line is kept. Raw HTML, images, reference links and
    /// entity references stay as typed, and nothing is fetched. Legacy
    /// marked-up text (`legacy_plain_text`) gets the same on its visible text,
    /// its directives left as they are. A file offer is shown as typed.
    pub fn render(&self, source: &str) -> Result<MessageDocument, Error> {
        if source.len() > MAX_CONTENT_BYTES {
            return Err(Error::TooLarge);
        }
        let marked = markdown::Marked::new(source);
        if markdown::literal(&marked.visible) {
            return Ok(MessageDocument {
                blocks: markup::expand(&markup::marked_pieces(source), 0, |_| Vec::new()),
            });
        }
        let mut translation = markdown::translate(&marked);
        let mut made = Vec::with_capacity(translation.blocks.len());
        let mut fences = 0;
        for (_, block, _) in &mut translation.blocks {
            made.push(match block {
                markdown::Block::Math { source, tex } => {
                    fences += 1;
                    ContentBlock::Math {
                        source: std::mem::take(source),
                        tex: std::mem::take(tex),
                    }
                }
                markdown::Block::Code { language, body } => {
                    let runs = self.highlight(body, language.as_deref())?;
                    ContentBlock::Code {
                        language: language.take(),
                        runs,
                    }
                }
            });
        }
        let (pieces, positions) = markdown::pieces(&marked, translation, made);
        // Past the block budget, the rest of the message as typed.
        let rest = |index| markdown::rest(&marked, &pieces, &positions, index);
        Ok(MessageDocument {
            blocks: markup::expand(&pieces, math::MAX_FORMULAS.saturating_sub(fences), rest),
        })
    }

    /// Legacy Mumble rich text becomes inert, marked-up text before it crosses
    /// back into the UI; no image, URL, style, script, font or other resource
    /// is loaded. See [`markup`] for what is kept and how.
    pub fn legacy_plain_text(&self, source: &str) -> Result<String, Error> {
        markup::legacy_text(source)
    }

    fn highlight(&self, source: &str, language: Option<&str>) -> Result<Vec<CodeRun>, Error> {
        let Highlighter { syntaxes, theme } = self.highlighter()?;
        let syntax = language
            .and_then(|token| syntaxes.find_syntax_by_token(token))
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, theme);
        let mut runs = Vec::new();
        for line in LinesWithEndings::from(source) {
            for (style, text) in highlighter.highlight_line(line, syntaxes)? {
                runs.push(CodeRun {
                    text: text.to_owned(),
                    foreground: [
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                        style.foreground.a,
                    ],
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                });
            }
        }
        Ok(runs)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "images"))]
    #[test]
    fn text_only_rendering_refuses_embedded_images_but_keeps_code() {
        let renderer = super::ContentRenderer::default();
        let html = "one<br>two<img src='data:image/png;base64,iVBORw0KGgo='><img src='https://tracker.invalid/x'>";
        let source = renderer.legacy_plain_text(html).unwrap();
        let document = renderer.render(&source).unwrap();
        assert!(
            document
                .blocks
                .iter()
                .all(|block| !matches!(block, nuschl_core::ContentBlock::Image { .. }))
        );
        assert!(document.blocks.iter().any(|block| matches!(block, nuschl_core::ContentBlock::Text { text, .. } if text.contains("one\ntwo") && text.contains("[image]"))));
        let code = renderer.render("```rust\nfn main() {}\n```").unwrap();
        assert!(
            matches!(code.blocks.first(), Some(nuschl_core::ContentBlock::Code { runs, .. }) if !runs.is_empty())
        );
    }

    /// A file offer sent as HTML (servers that allow it) is recognised from the
    /// document the worker renders, though its prepared source keeps markers.
    #[test]
    fn html_file_offers_are_recognised_after_rendering() -> Result<(), super::Error> {
        use nuschl_core::transfer::{Offer, format_offer, offer_in};
        let renderer = super::ContentRenderer::new()?;
        let offer = Offer {
            name: "frost.pdf".into(),
            size: 5_000_000,
            code: Some("2-disruptive-crumpled".into()),
            rendezvous: None,
        };
        let html = format_offer(&offer).html;
        let document = renderer.render(&renderer.legacy_plain_text(&html)?)?;
        assert_eq!(offer_in(&document), Some(offer.clone()));
        // The end of the offer names the same file, so it replaces the card.
        let ended = Offer {
            code: None,
            ..offer
        };
        let html = format_offer(&ended).html;
        let source = renderer.legacy_plain_text(&html)?;
        assert_eq!(nuschl_core::transfer::parse_offer(&source), None);
        assert_eq!(offer_in(&renderer.render(&source)?), Some(ended));
        Ok(())
    }

    use super::*;
    #[cfg(feature = "images")]
    use strip::png_chunk;

    #[test]
    #[cfg(feature = "images")]
    fn webp_images_decode_and_are_sent_as_jpeg_or_png() {
        let mut webp = Vec::new();
        let pixels =
            image::RgbaImage::from_fn(40, 30, |x, y| image::Rgba([x as u8, y as u8, 90, 255]));
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut std::io::Cursor::new(&mut webp), ImageFormat::WebP)
            .unwrap();
        let Some(ContentBlock::Image { width, height, .. }) = decode_image(&webp, 640, 480) else {
            panic!("the WebP image did not decode");
        };
        assert_eq!((width, height), (40, 30));
        let (html, _) = encode_image_message(&webp, 128 * 1024, ImageQuality::High).unwrap();
        assert!(
            html.contains("data:image/jpeg") || html.contains("data:image/png"),
            "{}",
            &html[..html.len().min(80)]
        );
    }
    use nuschl_core::EMBED;

    #[test]
    #[cfg(feature = "images")]
    fn favicons_decode_from_ico() {
        let mut ico = Vec::new();
        let pixels = vec![200u8; 128 * 128 * 4];
        image::codecs::ico::IcoEncoder::new(&mut ico)
            .write_image(&pixels, 128, 128, ExtendedColorType::Rgba8)
            .unwrap();
        let Some(ContentBlock::Image { width, height, .. }) = decode_icon(&ico) else {
            panic!("the icon did not decode");
        };
        assert_eq!((width, height), (64, 64));
        assert!(decode_icon(b"<!doctype html>").is_none());
    }

    #[test]
    fn legacy_html_is_inert_and_preserves_explicit_line_breaks() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        assert_eq!(renderer.legacy_plain_text("<p>one<br/><br/>two &amp; &lt;tag&gt;</p><script>secret()</script><img src='https://example.invalid/a'>")?, "one\n\ntwo & <tag>\n[image]");
        let source = "```rust<br/>fn main() {}<br/>```";
        assert!(matches!(
            renderer
                .render(&renderer.legacy_plain_text(source)?)?
                .blocks
                .first(),
            Some(ContentBlock::Code { .. })
        ));
        Ok(())
    }

    #[cfg(feature = "images")]
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([x as u8, y as u8, 128, 255])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    /// The chunk types of a PNG, in order.
    #[cfg(feature = "images")]
    fn chunk_types(png: &[u8]) -> Vec<String> {
        let mut types = Vec::new();
        let mut at = 8;
        while at + 8 <= png.len() {
            let length = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            types.push(String::from_utf8_lossy(&png[at + 4..at + 8]).into_owned());
            at += 12 + length;
        }
        types
    }

    #[test]
    #[cfg(feature = "images")]
    fn artwork_thumbnails_are_bare_rgba_pngs_with_the_source_pixels() {
        // A source PNG with a text chunk after its header.
        let source = png(8, 6);
        let mut tagged = source[..33].to_vec();
        tagged.extend(png_chunk(b"tEXt", b"Comment\0tracking id 1234"));
        tagged.extend_from_slice(&source[33..]);
        assert!(chunk_types(&tagged).contains(&"tEXt".to_owned()));

        let (width, height, thumbnail) = thumbnail_png(&tagged, 4, 4).unwrap();
        assert_eq!((width, height), (4, 3));
        assert!(
            chunk_types(&thumbnail)
                .iter()
                .all(|kind| ["IHDR", "IDAT", "IEND"].contains(&kind.as_str())),
            "{:?}",
            chunk_types(&thumbnail)
        );
        let decoded = image::load_from_memory(&thumbnail).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgba8);
        assert_eq!((decoded.width(), decoded.height()), (4, 3));
        // Unchanged size keeps every pixel as it was.
        let (_, _, same) = thumbnail_png(&source, 8, 8).unwrap();
        assert_eq!(
            image::load_from_memory(&same).unwrap().to_rgba8(),
            image::load_from_memory(&source).unwrap().to_rgba8()
        );
        assert!(thumbnail_png(b"not an image", 4, 4).is_none());
    }

    #[test]
    #[cfg(feature = "images")]
    fn inline_data_images_are_decoded_and_remote_images_ignored() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png(800, 400))
        );
        let html = format!(
            "look<br/><img src=\"{uri}\"/><img src=\"https://tracker.invalid/x.png\"/>done{}",
            EMBED
        );
        let text = renderer.legacy_plain_text(&html)?;
        let document = renderer.render(&text)?;
        let kinds: Vec<&str> = document
            .blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { .. } => "text",
                ContentBlock::Code { .. } => "code",
                ContentBlock::Image { .. } => "image",
                ContentBlock::Math { .. } => "math",
                ContentBlock::Break { .. } => "break",
            })
            .collect();
        assert_eq!(kinds, ["text", "image", "text"]);
        let Some(ContentBlock::Image { width, height, .. }) = document.blocks.get(1) else {
            panic!()
        };
        assert_eq!(
            (*width, *height),
            (360, 180),
            "thumbnail keeps the aspect ratio"
        );
        let Some(ContentBlock::Text { text, .. }) = document.blocks.get(2) else {
            panic!()
        };
        assert_eq!(
            text, "[image]done",
            "remote image is a placeholder; stray markers are stripped"
        );
        Ok(())
    }

    #[test]
    #[cfg(feature = "images")]
    fn images_pasted_in_stock_mumble_are_decoded() -> Result<(), Error> {
        // Log::imageToImg: base64, cut into 72-character lines, each
        // percent-encoded, joined with newlines.
        let encoded = base64::engine::general_purpose::STANDARD.encode(png(40, 30));
        let lines: Vec<String> = encoded
            .as_bytes()
            .chunks(72)
            .map(|line| {
                line.iter()
                    .map(|&byte| match byte {
                        b'+' => "%2B".to_string(),
                        b'/' => "%2F".to_string(),
                        b'=' => "%3D".to_string(),
                        other => (other as char).to_string(),
                    })
                    .collect()
            })
            .collect();
        assert!(
            lines.concat().contains('%'),
            "the fixture must exercise percent-encoding"
        );
        let html = format!(
            "<p><img src=\"data:image/PNG;base64,{}\" /></p>",
            lines.join("\n")
        );
        let renderer = ContentRenderer::new()?;
        let document = renderer.render(&renderer.legacy_plain_text(&html)?)?;
        assert!(
            document
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "{:?}",
            document
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Image { .. } => "image".to_string(),
                    ContentBlock::Text { text, .. } => text.chars().take(40).collect(),
                    _ => "other".into(),
                })
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    /// A screenshot-like image: flat areas with some text-like detail, or noise.
    #[cfg(feature = "images")]
    fn picture(width: u32, height: u32, noisy: bool) -> Vec<u8> {
        let mut state = 0x9e37_79b9_u32;
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if noisy {
                image::Rgb([
                    (x / 3) as u8 ^ state as u8,
                    (y / 2) as u8,
                    (state >> 8) as u8,
                ])
            } else if (x / 7 + y / 11) % 5 == 0 {
                image::Rgb([20, 20, 30])
            } else {
                image::Rgb([235, 235, 240])
            }
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// What a receiver (including stock Mumble's format) makes of an element.
    #[cfg(feature = "images")]
    fn received(element: &str) -> (u32, u32) {
        let renderer = ContentRenderer::default();
        let document = renderer
            .render(&renderer.legacy_plain_text(element).unwrap())
            .unwrap();
        let [ContentBlock::Image { source, .. }] = document.blocks.as_slice() else {
            panic!("{:?}", document.blocks.len())
        };
        let Some(ContentBlock::Image { width, height, .. }) = decode_view(source.as_ref().unwrap())
        else {
            panic!("the viewer cannot decode it")
        };
        (width, height)
    }

    #[test]
    #[cfg(feature = "images")]
    fn pasted_images_are_encoded_as_stock_mumble_does_within_the_limit() {
        let limit = 131_072;
        // A large, detailed picture: shrunk to 1600×1000, then by quality or size.
        let (element, thumbnail) =
            encode_image_message(&picture(2400, 1350, true), limit, ImageQuality::High).unwrap();
        assert!(element.len() <= limit, "{}", element.len());
        // With a thumbnail for the composer.
        let ContentBlock::Image {
            width: 128,
            height: 72,
            rgba,
            ..
        } = thumbnail
        else {
            panic!("a 128×72 thumbnail")
        };
        let pixels = base64::engine::general_purpose::STANDARD.decode(rgba);
        assert_eq!(pixels.map(|pixels| pixels.len()).ok(), Some(128 * 72 * 4));
        assert!(element.starts_with("<img src=\"data:image/jpeg;base64,"));
        assert!(element.ends_with("\" />"));
        let payload = &element["<img src=\"data:image/jpeg;base64,".len()..element.len() - 4];
        assert!(
            payload.contains('%'),
            "percent-encoded like Log::imageToImg"
        );
        let lines: Vec<&str> = payload.split('\n').collect();
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| {
            line.replace("%2B", "+")
                .replace("%2F", "/")
                .replace("%3D", "=")
                .len()
                <= 72
        }));
        let (width, height) = received(&element);
        assert!(width <= 1600 && height <= 1000, "{width}×{height}");
        assert!(
            (width * 9).abs_diff(height * 16) <= 16,
            "keeps 16:9: {width}×{height}"
        );

        // Flat screenshots stay lossless: PNG is smaller than JPEG there.
        let (element, _) =
            encode_image_message(&picture(800, 500, false), limit, ImageQuality::High).unwrap();
        assert!(
            element.starts_with("<img src=\"data:image/png;base64,"),
            "{}",
            &element[..40]
        );
        assert_eq!(received(&element), (800, 500));

        // A small limit shrinks the picture until it fits.
        let (element, _) =
            encode_image_message(&picture(1600, 1000, true), 20_000, ImageQuality::High).unwrap();
        assert!(element.len() <= 20_000);
        let (width, _) = received(&element);
        assert!(width < 1600, "{width}");
    }

    #[test]
    #[cfg(feature = "images")]
    fn low_and_high_fit_any_server_limit() {
        // Smooth, like a photo: both levels fit stock Murmur's limit as they are.
        let smooth = image::RgbImage::from_fn(2000, 1125, |x, y| {
            image::Rgb([(x / 10) as u8, (y / 6) as u8, ((x + y) / 15) as u8])
        });
        let smooth = encoded_jpeg(&smooth);
        let high = encode_image_message(&smooth, 131_072, ImageQuality::High)
            .unwrap()
            .0;
        let low = encode_image_message(&smooth, 131_072, ImageQuality::Low)
            .unwrap()
            .0;
        assert_eq!(received(&high), (1600, 900));
        assert_eq!(received(&low), (800, 450));
        assert!(low.len() < high.len(), "{} {}", low.len(), high.len());
        // Noise, and smaller server limits: smaller, never refused.
        let noisy = picture(640, 360, true);
        for quality in [ImageQuality::Low, ImageQuality::High] {
            for limit in [131_072, 16_384, 2_048, 600] {
                let element = encode_image_message(&noisy, limit, quality).unwrap().0;
                assert!(
                    element.len() <= limit,
                    "{quality:?} {limit}: {}",
                    element.len()
                );
                received(&element);
            }
        }
    }

    #[test]
    #[cfg(feature = "images")]
    fn sent_images_carry_none_of_the_original_metadata() {
        let pixels = image::RgbaImage::from_fn(48, 32, |x, y| {
            image::Rgba([(x * 5) as u8, ((y * 7) ^ (x * 13)) as u8, 90, 255])
        });
        let encoded = |format| {
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(pixels.clone())
                .write_to(&mut std::io::Cursor::new(&mut bytes), format)
                .unwrap();
            bytes
        };

        // JPEG: EXIF with GPS, XMP, an ICC profile and a comment, and data
        // after the end of the image.
        let segment = |marker: u8, body: &[u8]| {
            [
                &[0xff, marker][..],
                &(body.len() as u16 + 2).to_be_bytes(),
                body,
            ]
            .concat()
        };
        let mut rgb = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut rgb, 90)
            .write_image(
                image::DynamicImage::ImageRgba8(pixels.clone())
                    .to_rgb8()
                    .as_raw(),
                48,
                32,
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        let jpeg = [
            &rgb[..2],
            &segment(
                0xe1,
                b"Exif\0\0MM\0*\0\0\0\x08NUSCHL-GPS 48.2082N 16.3738E Pixel 9",
            ),
            &segment(
                0xe1,
                b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>NUSCHL-XMP</x:xmpmeta>",
            ),
            &segment(0xe2, b"ICC_PROFILE\0\x01\x01NUSCHL-ICC"),
            &segment(0xfe, b"NUSCHL-COMMENT"),
            &rgb[2..],
            b"NUSCHL-TRAILER",
        ]
        .concat();

        // PNG: text, XMP, EXIF and time chunks, and data after IEND.
        let source = encoded(ImageFormat::Png);
        let png = [
            &source[..33],
            &png_chunk(b"tEXt", b"Comment\0NUSCHL-COMMENT"),
            &png_chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0NUSCHL-XMP"),
            &png_chunk(b"eXIf", b"MM\0*\0\0\0\x08NUSCHL-GPS"),
            &png_chunk(b"tIME", &[0x07, 0xea, 11, 3, 14, 30, 12]),
            &source[33..],
            b"NUSCHL-TRAILER",
        ]
        .concat();

        // WebP: the extended format with EXIF and XMP chunks.
        let lossless = encoded(ImageFormat::WebP);
        let riff = |kind: &[u8; 4], data: &[u8]| {
            let pad = vec![0; data.len() % 2];
            [
                kind.as_slice(),
                &(data.len() as u32).to_le_bytes(),
                data,
                &pad,
            ]
            .concat()
        };
        // Alpha, EXIF and XMP; then the canvas size, less one, in 24 bits each.
        let header = [
            &[0x1c, 0, 0, 0][..],
            &47u32.to_le_bytes()[..3],
            &31u32.to_le_bytes()[..3],
        ]
        .concat();
        let body = [
            &b"WEBP"[..],
            &riff(b"VP8X", &header),
            &lossless[12..],
            &riff(b"EXIF", b"MM\0*\0\0\0\x08NUSCHL-GPS"),
            &riff(b"XMP ", b"<x:xmpmeta>NUSCHL-XMP</x:xmpmeta>"),
        ]
        .concat();
        let webp = [&b"RIFF"[..], &(body.len() as u32).to_le_bytes(), &body].concat();

        // GIF: a comment extension after the global colour table.
        let source = encoded(ImageFormat::Gif);
        let table = if source[10] & 0x80 != 0 {
            3 << ((source[10] & 7) + 1)
        } else {
            0
        };
        let comment = [&[0x21, 0xfe, 14][..], b"NUSCHL-COMMENT", &[0]].concat();
        let gif = [&source[..13 + table], &comment, &source[13 + table..]].concat();

        // A photo's EXIF: camera model, capture time and GPS position.
        let photo = jpeg_with_exif(
            &DynamicImage::ImageRgba8(pixels.clone()).to_rgb8(),
            &exif(1),
        );
        let inputs = [
            ("JPEG", jpeg),
            ("EXIF", photo),
            ("PNG", png),
            ("WebP", webp),
            ("GIF", gif),
        ];
        let qualities = [ImageQuality::Low, ImageQuality::High];
        for ((format, input), quality) in inputs
            .iter()
            .flat_map(|input| qualities.map(|quality| (input, quality)))
        {
            let format = format!("{format} {quality:?}");
            assert!(input.windows(6).any(|window| window == b"NUSCHL"));
            let (element, _) = encode_image_message(input, 131_072, quality)
                .unwrap_or_else(|error| panic!("{format}: {error}"));
            let source = element
                .strip_prefix("<img src=\"")
                .unwrap()
                .strip_suffix("\" />")
                .unwrap();
            let (sent, _) = nuschl_core::data_uri_image(source).unwrap();
            assert!(
                !sent.windows(6).any(|window| window == b"NUSCHL"),
                "{format}"
            );
            assert!(
                !sent.windows(10).any(|window| window == b"2026:11:03"),
                "{format}"
            );
            if sent.starts_with(b"\x89PNG\r\n\x1a\n") {
                let chunks = chunk_types(&sent);
                assert!(
                    chunks
                        .iter()
                        .all(|kind| ["IHDR", "IDAT", "IEND"].contains(&kind.as_str())),
                    "{format}: {chunks:?}"
                );
                assert!(sent.ends_with(&png_chunk(b"IEND", b"")), "{format}");
            } else {
                assert!(sent.starts_with(&[0xff, 0xd8]), "{format}");
                // The segments up to the scan: no APP1–APP15 and no comment.
                let mut at = 2;
                while sent[at + 1] != 0xda {
                    let marker = sent[at + 1];
                    assert!(
                        !(0xe1..=0xef).contains(&marker) && marker != 0xfe,
                        "{format}: {marker:#x}"
                    );
                    at += 2 + u16::from_be_bytes([sent[at + 2], sent[at + 3]]) as usize;
                }
                assert!(sent.ends_with(&[0xff, 0xd9]), "{format}");
            }
        }
    }

    #[test]
    #[cfg(feature = "images")]
    fn transparent_pixels_carry_no_colour_after_re_encoding() {
        // Left half: a pattern hidden under alpha 0; right half: opaque blue.
        // Wider than 1600 pixels, so it is resized on the way.
        let hidden = |x: u32, y: u32| image::Rgba([200, (x * 7) as u8, (y * 13) as u8, 0]);
        let pixels = image::RgbaImage::from_fn(2000, 400, |x, y| {
            if x < 1000 {
                hidden(x, y)
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(pixels.clone())
            .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        // Only blue is visible: no red or green anywhere a pixel shows, and
        // no colour at all where none does.
        let blue_only = |image: &image::RgbaImage| {
            image.pixels().all(|pixel| match pixel.0 {
                [_, _, _, 0] => pixel.0 == [0; 4],
                [red, green, _, _] => red == 0 && green == 0,
            })
        };

        let (element, _) = encode_image_message(&png, 131_072, ImageQuality::High).unwrap();
        assert!(element.starts_with("<img src=\"data:image/png;base64,"));
        let source = element
            .strip_prefix("<img src=\"")
            .unwrap()
            .strip_suffix("\" />")
            .unwrap();
        let sent = image::load_from_memory(&nuschl_core::data_uri_image(source).unwrap().0)
            .unwrap()
            .to_rgba8();
        assert_eq!(sent.dimensions(), (1600, 320));
        assert!(blue_only(&sent));

        // Unresized, resized and as a thumbnail.
        let clean = visible(DynamicImage::ImageRgba8(pixels));
        assert!(blue_only(&clean.to_rgba8()));
        for (width, height) in [(500, 100), (128, 128)] {
            let resized = fit(clean.clone(), width, height).to_rgba8();
            assert!(blue_only(&resized), "{width}×{height}");
            // The edge shows blue fading out, not the hidden colour.
            let edge = resized.get_pixel(resized.width() / 2 - 1, 1).0;
            assert!(edge[3] > 0 && edge[3] < 255 && edge[2] == 255, "{edge:?}");
        }

        // JPEG blends onto white: the transparent half is white, the rest
        // white, blue or between them.
        let jpeg = encode_jpeg(&flatten(&clean), 85).unwrap();
        let sent = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert!(sent.pixels().all(|pixel| {
            let [red, green, blue] = pixel.0.map(i16::from);
            (red - green).abs() <= 12 && blue >= red.max(green) - 12
        }));
        assert!(
            sent.get_pixel(10, 10)
                .0
                .iter()
                .all(|&channel| channel >= 250)
        );
    }

    /// EXIF as a phone writes it (TIFF, big-endian): the camera model, the
    /// capture time, the GPS position and `orientation`.
    #[cfg(feature = "images")]
    fn exif(orientation: u16) -> Vec<u8> {
        let entry = |tag: u16, kind: u16, count: u32, value: u32| {
            [
                &tag.to_be_bytes()[..],
                &kind.to_be_bytes(),
                &count.to_be_bytes(),
                &value.to_be_bytes(),
            ]
            .concat()
        };
        let (ascii, short, long, rational) = (2, 3, 4, 5);
        let ifd = |entries: &[Vec<u8>]| {
            [
                &(entries.len() as u16).to_be_bytes()[..],
                &entries.concat(),
                &[0; 4],
            ]
            .concat()
        };
        let rationals = |values: [(u32, u32); 3]| -> Vec<u8> {
            values
                .iter()
                .flat_map(|(numerator, denominator)| {
                    [numerator.to_be_bytes(), denominator.to_be_bytes()].concat()
                })
                .collect()
        };
        let model = b"NUSCHL-MODEL Pixel 9\0";
        let time = b"2026:11:03 14:30:12\0";
        // IFD0 at 8, then its strings, the Exif IFD, its string, the GPS IFD
        // and its coordinates.
        let model_at = 8 + 2 + 5 * 12 + 4;
        let time_at = model_at + model.len() as u32;
        let exif_at = time_at + time.len() as u32;
        let original_at = exif_at + 2 + 12 + 4;
        let gps_at = original_at + time.len() as u32;
        let latitude_at = gps_at + 2 + 4 * 12 + 4;
        let longitude_at = latitude_at + 24;
        [
            &b"MM\0*\0\0\0\x08"[..],
            &ifd(&[
                entry(0x0110, ascii, model.len() as u32, model_at),
                entry(0x0112, short, 1, u32::from(orientation) << 16),
                entry(0x0132, ascii, time.len() as u32, time_at),
                entry(0x8769, long, 1, exif_at),
                entry(0x8825, long, 1, gps_at),
            ]),
            model,
            time,
            &ifd(&[entry(0x9003, ascii, time.len() as u32, original_at)]),
            time,
            &ifd(&[
                entry(0x0001, ascii, 2, u32::from_be_bytes(*b"N\0\0\0")),
                entry(0x0002, rational, 3, latitude_at),
                entry(0x0003, ascii, 2, u32::from_be_bytes(*b"E\0\0\0")),
                entry(0x0004, rational, 3, longitude_at),
            ]),
            &rationals([(48, 1), (12, 1), (2952, 100)]),
            &rationals([(16, 1), (22, 1), (2568, 100)]),
        ]
        .concat()
    }

    /// A JPEG of `pixels` with `exif` in its APP1 segment.
    #[cfg(feature = "images")]
    fn jpeg_with_exif(pixels: &image::RgbImage, exif: &[u8]) -> Vec<u8> {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                ExtendedColorType::Rgb8,
            )
            .unwrap();
        let app1 = [&b"Exif\0\0"[..], exif].concat();
        [
            &jpeg[..2],
            &[0xff, 0xe1],
            &(app1.len() as u16 + 2).to_be_bytes(),
            &app1,
            &jpeg[2..],
        ]
        .concat()
    }

    /// The pixels a sent `<img>` element carries.
    #[cfg(feature = "images")]
    fn sent_pixels(element: &str) -> image::RgbImage {
        let source = element
            .strip_prefix("<img src=\"")
            .and_then(|element| element.strip_suffix("\" />"))
            .unwrap();
        let (bytes, _) = nuschl_core::data_uri_image(source).unwrap();
        image::load_from_memory(&bytes).unwrap().to_rgb8()
    }

    #[test]
    #[cfg(feature = "images")]
    fn photos_are_sent_upright() {
        // A landscape picture, red on the left and blue on the right, that a
        // phone held upright stored turned: EXIF orientation 6 (turn it 90°
        // clockwise to show it).
        let pixels = image::RgbImage::from_fn(64, 32, |x, _| {
            if x < 32 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        });
        let (element, _) = encode_image_message(
            &jpeg_with_exif(&pixels, &exif(6)),
            131_072,
            ImageQuality::High,
        )
        .unwrap();
        let sent = sent_pixels(&element);
        assert_eq!(sent.dimensions(), (32, 64));
        // Red above, blue below.
        assert!(sent.get_pixel(16, 8)[0] > 200 && sent.get_pixel(16, 8)[2] < 60);
        assert!(sent.get_pixel(16, 56)[2] > 200 && sent.get_pixel(16, 56)[0] < 60);

        // Upright already (1), or without EXIF: as stored.
        for input in [jpeg_with_exif(&pixels, &exif(1)), encoded_jpeg(&pixels)] {
            let (element, _) = encode_image_message(&input, 131_072, ImageQuality::High).unwrap();
            assert_eq!(sent_pixels(&element).dimensions(), (64, 32));
        }

        // A large one shrinks into the upright 1600×1000 box: 2400×1350
        // stored, 1350×2400 upright, sent at 563×1000.
        let large = image::RgbImage::from_fn(2400, 1350, |x, y| image::Rgb([x as u8, y as u8, 0]));
        let (element, _) = encode_image_message(
            &jpeg_with_exif(&large, &exif(8)),
            1_000_000,
            ImageQuality::High,
        )
        .unwrap();
        assert_eq!(sent_pixels(&element).dimensions(), (563, 1000));
    }

    #[cfg(feature = "images")]
    fn encoded_jpeg(pixels: &image::RgbImage) -> Vec<u8> {
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(pixels.clone())
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        jpeg
    }

    #[test]
    #[cfg(feature = "images")]
    fn originals_keep_their_pixels_and_lose_their_metadata() {
        let pixels = image::RgbaImage::from_fn(48, 32, |x, y| {
            image::Rgba([(x * 5) as u8, ((y * 7) ^ (x * 13)) as u8, 90, 255])
        });
        let encoded = |format| {
            let mut bytes = Vec::new();
            DynamicImage::ImageRgba8(pixels.clone())
                .write_to(&mut std::io::Cursor::new(&mut bytes), format)
                .unwrap();
            bytes
        };
        let segment = |marker: u8, body: &[u8]| {
            [
                &[0xff, marker][..],
                &(body.len() as u16 + 2).to_be_bytes(),
                body,
            ]
            .concat()
        };
        // Turned (EXIF orientation 6), with a camera model, capture time and
        // GPS position.
        let turned = exif(6);

        // JPEG: EXIF, XMP, a comment and a second picture's index (MPF),
        // an ICC profile, and data after the end of the image.
        let photo = jpeg_with_exif(&DynamicImage::ImageRgba8(pixels.clone()).to_rgb8(), &turned);
        let jpeg = [
            &photo[..2],
            &segment(
                0xe1,
                b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>NUSCHL-XMP</x:xmpmeta>",
            ),
            &segment(0xe2, b"MPF\0NUSCHL-MPF"),
            &segment(0xe2, b"ICC_PROFILE\0\x01\x01KEEP-ICC"),
            &segment(0xfe, b"NUSCHL-COMMENT"),
            &photo[2..],
            b"NUSCHL-TRAILER",
        ]
        .concat();

        // PNG: EXIF, text, XMP and time chunks, and data after IEND.
        let source = encoded(ImageFormat::Png);
        let png = [
            &source[..33],
            &png_chunk(b"eXIf", &turned),
            &png_chunk(b"tEXt", b"Comment\0NUSCHL-COMMENT"),
            &png_chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0NUSCHL-XMP"),
            &png_chunk(b"tIME", &[0x07, 0xea, 11, 3, 14, 30, 12]),
            &source[33..],
            b"NUSCHL-TRAILER",
        ]
        .concat();

        // WebP: the extended format with an ICC profile, EXIF and XMP, and
        // data after the RIFF container.
        let lossless = encoded(ImageFormat::WebP);
        let riff = |kind: &[u8; 4], data: &[u8]| {
            let pad = vec![0; data.len() % 2];
            [
                kind.as_slice(),
                &(data.len() as u32).to_le_bytes(),
                data,
                &pad,
            ]
            .concat()
        };
        // ICC, alpha, EXIF and XMP; then the canvas size, less one, in 24 bits each.
        let header = [
            &[0x3c, 0, 0, 0][..],
            &47u32.to_le_bytes()[..3],
            &31u32.to_le_bytes()[..3],
        ]
        .concat();
        let body = [
            &b"WEBP"[..],
            &riff(b"VP8X", &header),
            &riff(b"ICCP", b"KEEP-ICC"),
            &lossless[12..],
            &riff(b"EXIF", &turned),
            &riff(b"XMP ", b"<x:xmpmeta>NUSCHL-XMP</x:xmpmeta>"),
        ]
        .concat();
        let webp = [
            &b"RIFF"[..],
            &(body.len() as u32).to_le_bytes(),
            &body,
            b"NUSCHL-TRAILER",
        ]
        .concat();

        // GIF: looping, a comment and XMP, and data after the trailer.
        let source = encoded(ImageFormat::Gif);
        let table = if source[10] & 0x80 != 0 {
            3 << ((source[10] & 7) + 1)
        } else {
            0
        };
        let looping = [&[0x21, 0xff, 11][..], b"NETSCAPE2.0", &[3, 1, 0, 0, 0]].concat();
        let comment = [&[0x21, 0xfe, 14][..], b"NUSCHL-COMMENT", &[0]].concat();
        let xmp = [
            &[0x21, 0xff, 11][..],
            b"XMP DataXMP",
            &[10],
            b"NUSCHL-XMP",
            &[0],
        ]
        .concat();
        let gif = [
            &source[..13 + table],
            &looping,
            &comment,
            &xmp,
            &source[13 + table..],
            b"NUSCHL-TRAILER",
        ]
        .concat();

        // BMP: data after the size its header gives.
        let bmp = [&encoded(ImageFormat::Bmp)[..], b"NUSCHL-TRAILER"].concat();

        let decoder = |bytes: &[u8]| {
            ImageReader::new(std::io::Cursor::new(bytes.to_vec()))
                .with_guessed_format()
                .unwrap()
                .into_decoder()
                .unwrap()
        };
        let contains = |bytes: &[u8], text: &[u8]| bytes.windows(text.len()).any(|w| w == text);
        for (format, input, upright) in [
            (ImageFormat::Jpeg, &jpeg, false),
            (ImageFormat::Png, &png, false),
            (ImageFormat::WebP, &webp, false),
            (ImageFormat::Gif, &gif, true),
            (ImageFormat::Bmp, &bmp, true),
        ] {
            assert!(contains(input, b"NUSCHL"), "{format:?}");
            let stripped = strip_metadata(input).unwrap_or_else(|| panic!("{format:?}"));
            for gone in [&b"NUSCHL"[..], b"2026:11:03"] {
                assert!(!contains(&stripped, gone), "{format:?}");
            }
            // The same format and pixels, as stored.
            assert_eq!(image::guess_format(&stripped).unwrap(), format);
            assert_eq!(
                image::load_from_memory(&stripped).unwrap().to_rgba8(),
                image::load_from_memory(input).unwrap().to_rgba8(),
                "{format:?}"
            );
            // Turned as before, by EXIF that holds only that.
            let mut decoder = decoder(&stripped);
            if upright {
                assert_eq!(decoder.exif_metadata().unwrap(), None, "{format:?}");
            } else {
                assert_eq!(
                    decoder.orientation().unwrap(),
                    Orientation::Rotate90,
                    "{format:?}"
                );
                let exif = decoder.exif_metadata().unwrap().unwrap();
                assert_eq!(
                    exif,
                    b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0"
                );
            }
        }
        // The colour profile and the looping stay.
        assert!(contains(
            &strip_metadata(&jpeg).unwrap(),
            b"ICC_PROFILE\0\x01\x01KEEP-ICC"
        ));
        assert!(contains(
            &strip_metadata(&webp).unwrap(),
            b"ICCP\x08\0\0\0KEEP-ICC"
        ));
        assert!(contains(&strip_metadata(&gif).unwrap(), b"NETSCAPE2.0"));

        // Upright already: no EXIF at all.
        let upright = jpeg_with_exif(
            &DynamicImage::ImageRgba8(pixels.clone()).to_rgb8(),
            &exif(1),
        );
        let stripped = strip_metadata(&upright).unwrap();
        assert_eq!(decoder(&stripped).exif_metadata().unwrap(), None);
        assert!(!contains(&stripped, b"Exif"));

        // Other formats, and broken files, are not stripped; no cut-off
        // file panics.
        for input in [&jpeg, &png, &webp, &gif, &bmp] {
            for end in 0..input.len() {
                strip_metadata(&input[..end]);
            }
        }
        assert_eq!(strip_metadata(b"II*\0\x08\0\0\0"), None);
        assert_eq!(strip_metadata(&encoded(ImageFormat::Png)[..60]), None);
        assert_eq!(strip_metadata(&photo[..photo.len() - 2]), None);
    }

    #[test]
    #[cfg(feature = "images")]
    fn pasted_images_that_cannot_fit_or_decode_are_rejected() {
        assert!(matches!(
            encode_image_message(&picture(64, 64, true), 40, ImageQuality::High),
            Err(Error::ImageTooLarge(40))
        ));
        assert!(matches!(
            encode_image_message(b"not an image", 131_072, ImageQuality::High),
            Err(Error::InvalidImage)
        ));
        assert!(matches!(
            encode_image_message(&picture(64, 64, false)[..40], 131_072, ImageQuality::High),
            Err(Error::InvalidImage)
        ));
        assert!(matches!(
            encode_image_message(
                b"<svg xmlns='http://www.w3.org/2000/svg'/>",
                131_072,
                ImageQuality::High
            ),
            Err(Error::InvalidImage)
        ));
    }

    #[test]
    fn plain_messages_never_load_the_highlighter() {
        let renderer = ContentRenderer::default();
        renderer.render("no code here").unwrap();
        renderer.legacy_plain_text("<b>nor here</b>").unwrap();
        assert!(renderer.highlighter.get().is_none());
        renderer.render("```\ncode\n```").unwrap();
        assert!(renderer.highlighter.get().is_some());
    }

    #[test]
    #[cfg(feature = "images")]
    fn hostile_image_payloads_are_rejected() {
        assert!(decode_image(b"not an image", 10, 10).is_none());
        assert!(
            decode_image(&png(1, 1)[..20], 10, 10).is_none(),
            "truncated"
        );
        assert!(
            data_uri("data:image/svg+xml;base64,PHN2Zz4=").is_none(),
            "SVG is not accepted"
        );
        assert!(data_uri("https://example.invalid/a.png").is_none());
    }

    #[test]
    #[cfg(feature = "images")]
    fn images_without_pixels_are_rejected() {
        // Found by fuzzing: a GIF whose screen is 110 × 0 pixels became an
        // image block of that size without any pixels.
        let gif = base64::engine::general_purpose::STANDARD
            .decode("R0lGODlhbgAAAAAAACwBAAACAQAEAIB4RwAAACwBiVA=")
            .unwrap();
        assert!(decode_image(&gif, 640, 480).is_none());
        assert!(decode_icon(&gif).is_none());
        let uri = format!(
            "data:image/gif;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&gif)
        );
        assert!(decode_view(&uri).is_none());
        assert!(matches!(
            encode_image_message(&gif, 131_072, ImageQuality::High),
            Err(Error::InvalidImage)
        ));
    }

    #[test]
    fn literal_messages_keep_blank_lines_and_html_inert() -> Result<(), Error> {
        let source = "first\n\nsecond\n<script>never execute</script>";
        let result = ContentRenderer::new()?.render(source)?;
        assert_eq!(result.blocks, vec![ContentBlock::text(source)]);
        Ok(())
    }

    #[test]
    fn a_leading_line_break_before_a_fence_is_dropped() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let plain = renderer.legacy_plain_text("<br/>```rust<br/>fn main() {}<br/>```")?;
        let document = renderer.render(&plain)?;
        assert!(
            matches!(document.blocks.first(), Some(ContentBlock::Code { .. })),
            "{document:?}"
        );
        Ok(())
    }

    #[test]
    fn rust_code_is_highlighted_and_retains_its_newlines() -> Result<(), Error> {
        let result = ContentRenderer::new()?.render("before\n```rust\nfn main() {}\n```\nafter")?;
        let Some(ContentBlock::Code { language, runs }) = result.blocks.get(1) else {
            panic!("expected a code block between the two text blocks");
        };
        assert_eq!(language.as_deref(), Some("rust"));
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "fn main() {}\n"
        );
        assert!(
            runs.windows(2)
                .any(|pair| pair[0].foreground != pair[1].foreground)
        );
        Ok(())
    }

    #[test]
    fn nix_grammar_styles_constructs_without_changing_code_or_plain_fallback() -> Result<(), Error>
    {
        let renderer = ContentRenderer::new()?;
        let body = "# comment 🦀\n{ pkgs, ... }:\nlet greeting = \"hello ${pkgs.lib.version}\"; in rec {\n\n  enabled = true;\n  path = ./module.nix;\n  literal = \"<script>not executed</script>\";\n}\n";
        let runs = renderer.highlight(body, Some("nix"))?;
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            body
        );
        let color = |needle: &str| {
            let offset = body.find(needle).unwrap();
            let mut start = 0;
            runs.iter()
                .find_map(|run| {
                    let range = start..start + run.text.len();
                    start = range.end;
                    range.contains(&offset).then_some(run.foreground)
                })
                .unwrap()
        };
        assert_ne!(color("# comment"), color("let greeting"));
        assert_ne!(color("hello"), color("let greeting"));
        // This theme gives attribute names and booleans the same orange.
        assert_ne!(color("true"), color("let greeting"));
        for language in [Some("nix"), Some("Nix"), Some("unknown_language"), None] {
            let rendered = renderer.highlight(body, language)?;
            assert_eq!(
                rendered
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>(),
                body
            );
            if language == Some("unknown_language") || language.is_none() {
                assert!(
                    rendered
                        .iter()
                        .all(|run| run.foreground == rendered[0].foreground)
                );
            } else {
                assert!(
                    rendered
                        .iter()
                        .any(|run| run.foreground != rendered[0].foreground),
                    "Nix token case must retain syntax styles"
                );
            }
        }
        let fence = "```nix\n";
        let tail = "\n```";
        let long = format!(
            "{fence}#{}{tail}",
            "x".repeat(MAX_CONTENT_BYTES - fence.len() - tail.len() - 1)
        );
        let bounded = renderer.render(&long)?;
        assert!(bounded.within_limits());
        let Some(ContentBlock::Code { runs, .. }) = bounded.blocks.first() else {
            panic!("bounded Nix comment should remain code");
        };
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            &long[fence.len()..long.len() - 3]
        );
        assert!(matches!(
            renderer.render(&"x".repeat(MAX_CONTENT_BYTES + 1)),
            Err(Error::TooLarge)
        ));
        Ok(())
    }

    #[test]
    fn code_blocks_count_towards_the_block_limit() -> Result<(), Error> {
        // 256 KiB of fences were 32,766 code blocks, each laid out by the
        // desktop; blocks are capped at 512 per message.
        let source = "```\n".repeat(MAX_CONTENT_BYTES / 4);
        let document = ContentRenderer::new()?.render(&source)?;
        let code = |block: &&ContentBlock| matches!(block, ContentBlock::Code { .. });
        assert!(document.blocks.iter().filter(code).count() > 200);
        let blocks = document
            .blocks
            .iter()
            .filter(|block| !block.is_break())
            .count();
        assert!(
            blocks <= markup::MAX_BLOCKS && document.within_limits(),
            "{blocks}"
        );
        Ok(())
    }

    #[test]
    fn rejects_large_messages_before_parsing() -> Result<(), Error> {
        assert!(matches!(
            ContentRenderer::new()?.render(&"x".repeat(MAX_CONTENT_BYTES + 1)),
            Err(Error::TooLarge)
        ));
        Ok(())
    }

    #[test]
    fn hostile_markup_and_images_never_panic() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let fragments = [
            "<",
            ">",
            "<img src=\"data:image/png;base64,",
            "\"/>",
            "```",
            "rust\n",
            "<br/>",
            "&amp;",
            "&#x0;",
            "<script>",
            "</p>",
            "<table><tr><td>",
            "\u{FFFC}",
            "data:image/gif;base64,R0lGODlh",
            "AAAA",
            "\n\n",
            "<b><i><u>",
            "<a href='javascript:x'>",
            "~~~",
            "`",
            "\u{202E}",
            "<!--",
            "-->",
            "<![CDATA[",
            "<a href=\"https://x.example/(a)\">",
            "</a>",
            "https://",
            "mumble://h",
            ").",
            "\u{FFFC}link:https://y.example\u{FFFC}",
            "\u{FFFC}/link\u{FFFC}",
            "<ul><li>",
            "</li>",
            "<ol start='9'>",
            "<blockquote>",
            "<pre>",
            "</pre>",
            "<h1>",
            "<hr>",
            "<th>",
            "<font color='#abc'>",
            "<span style='color:rgb(1,2,3);font-weight:700;text-decoration:underline line-through'>",
            "\u{FFFC}item:9:\u{FFFC}",
            "\u{FFFC}quote\u{FFFC}",
            "\u{FFFC}h2\u{FFFC}",
        ];
        let mut state = 99_u64;
        for _ in 0..3_000 {
            let mut text = String::new();
            for _ in 0..(state % 24) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                text.push_str(fragments[(state % fragments.len() as u64) as usize]);
            }
            let plain = renderer.legacy_plain_text(&text)?;
            for document in [renderer.render(&plain)?, renderer.render(&text)?] {
                for block in document.blocks {
                    if let ContentBlock::Text { text, spans, .. } = block {
                        let mut end = 0;
                        for span in spans {
                            assert!(end <= span.start && span.start < span.end);
                            assert!(text.is_char_boundary(span.start));
                            end = span.end;
                        }
                        assert!(end <= text.len() && text.is_char_boundary(end));
                    }
                }
            }
        }
        // Image decoders reject arbitrary bytes behind valid magic numbers.
        #[cfg(feature = "images")]
        for magic in [&b"\x89PNG\r\n\x1a\n"[..], b"\xff\xd8\xff", b"GIF89a"] {
            for length in [0, 1, 7, 64, 512] {
                let mut bytes = magic.to_vec();
                bytes.extend((0..length).map(|index| (index * 37 % 251) as u8));
                assert!(decode_image(&bytes, 64, 64).is_none());
            }
        }
        Ok(())
    }
}
