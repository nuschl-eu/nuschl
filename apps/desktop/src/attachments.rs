// SPDX-License-Identifier: EUPL-1.2
//! What the composer sends along with its text. Mumble has no file transfer:
//! images go as stock Mumble sends them, an `<img>` in the message, one per
//! message; other files, and images at Original quality, are offered for a
//! peer-to-peer transfer.
use gpui_kit::RenderImage;
use nuschl_core::ImageQuality;
use std::{
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

pub enum Attachment {
    /// Sent as its message's [`Choice`] has it prepared (see [`plan`] and
    /// [`settle`]), and prepared again from `source` when that changes.
    Image {
        id: u64,
        name: String,
        source: Source,
        prepared: Option<Prepared>,
    },
    File {
        id: u64,
        name: String,
        path: PathBuf,
        size: u64,
    },
}

impl Attachment {
    pub fn id(&self) -> u64 {
        match self {
            Self::Image { id, .. } | Self::File { id, .. } => *id,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::Image { name, .. } | Self::File { name, .. } => name,
        }
    }
    /// Images wait for the content worker; files are ready at once.
    pub fn ready(&self) -> bool {
        !matches!(self, Self::Image { prepared: None, .. })
    }
}

/// Where an attached image comes from: a file is read when its turn comes.
#[derive(Clone)]
pub enum Source {
    Bytes(Arc<Vec<u8>>),
    /// A file of `size` bytes; `unsupported` names a format the content
    /// worker does not read (HEIC, AVIF, TIFF, JPEG XL, SVG).
    File {
        path: PathBuf,
        size: u64,
        unsupported: Option<&'static str>,
    },
}

/// The ids of the images in `tray`.
pub fn images(tray: &[Attachment]) -> Vec<u64> {
    tray.iter()
        .filter(|attachment| matches!(attachment, Attachment::Image { .. }))
        .map(Attachment::id)
        .collect()
}

/// What an image goes as.
pub enum Prepared {
    /// An `<img>` element in the message, and its thumbnail.
    Inline(String, Arc<RenderImage>),
    /// A file offered for transfer, of this size.
    File(Offered, u64),
}

impl Prepared {
    /// The bytes it adds to what is sent: the element, or the file.
    pub fn size(&self) -> u64 {
        match self {
            Self::Inline(html, _) => html.len() as u64,
            Self::File(_, size) => *size,
        }
    }
}

/// A file to offer, under the name it is offered with. A staged copy (an
/// original without its metadata, or a pasted image) is deleted with this:
/// when it leaves the tray, or when its offer ends.
pub struct Offered {
    pub name: String,
    pub path: PathBuf,
    staged: bool,
}

impl Offered {
    pub fn file(name: String, path: PathBuf) -> Self {
        Self {
            name,
            path,
            staged: false,
        }
    }
}

impl Drop for Offered {
    fn drop(&mut self) {
        if self.staged {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// What a message's images go as. Each new message starts with Strip
/// metadata ticked and the quality from Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub strip_metadata: bool,
    pub quality: ImageQuality,
}

/// How an image is prepared.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Re-encoded by the content worker within the server's image limit.
    Encode(ImageQuality),
    /// Without its metadata, by the content worker, and offered as a file.
    Strip,
    /// Offered as a file as it is.
    AsIs,
}

/// How an image from `source` goes for `choice`, where `transfer` says
/// whether files can be offered and `images` whether the server takes
/// images. Original needs the transfer; without it, the image goes
/// re-encoded at High. An error says why it cannot go (see [`settle`]).
pub fn plan(source: &Source, choice: Choice, transfer: bool, images: bool) -> Result<Plan, String> {
    let unsupported = match source {
        Source::File { unsupported, .. } => *unsupported,
        Source::Bytes(_) => None,
    };
    match (choice.quality, unsupported) {
        (ImageQuality::Original, _) if transfer && !choice.strip_metadata => Ok(Plan::AsIs),
        (ImageQuality::Original, Some(format)) if transfer => Err(format!(
            "{format} metadata cannot be removed from the original"
        )),
        (ImageQuality::Original, None) if transfer => Ok(Plan::Strip),
        _ if !images => Err("this server does not accept images".into()),
        (_, Some(format)) => Err(format!("{format} cannot be re-encoded")),
        (ImageQuality::Low, None) => Ok(Plan::Encode(ImageQuality::Low)),
        (_, None) => Ok(Plan::Encode(ImageQuality::High)),
    }
}

/// What an image was prepared as.
pub enum Ready {
    /// Re-encoded by the content worker.
    Encoded(nuschl_ipc::EncodedImage),
    /// The original file, as it is.
    Original,
    /// A copy in the staging folder, and its size.
    Staged(PathBuf, u64),
}

/// Images one message may carry. Each keeps up to 256 KiB to send and a
/// thumbnail; a hundred take two minutes to leave at stock Murmur's rate limit.
pub const MAX_IMAGES: usize = 100;
/// An image's base64 must fit one content worker frame (`MAX_FRAME_BYTES`).
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Room kept beside every image for the text, which goes with the first.
pub const CAPTION_ROOM: usize = 1024;
/// Between the text and the first image.
const BREAK: &str = "<br/>";

/// What a dropped or chosen path can be sent as.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    /// PNG, JPEG, GIF, BMP or WebP.
    Image {
        size: u64,
    },
    /// An image in a format the content worker does not re-encode.
    Unsupported {
        size: u64,
        format: &'static str,
    },
    File {
        size: u64,
    },
}

/// Why a path cannot be attached.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Folder,
    Unreadable(String),
}

/// Whether `path` can go as an image, judged from its first bytes (the
/// content worker decodes the rest), or else as a file.
pub fn check(path: &Path) -> Result<Kind, Refusal> {
    let unreadable = |error: std::io::Error| Refusal::Unreadable(error.to_string());
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let metadata = file.metadata().map_err(unreadable)?;
    if metadata.is_dir() {
        return Err(Refusal::Folder);
    }
    let size = metadata.len();
    let mut head = Vec::with_capacity(512);
    file.take(512).read_to_end(&mut head).map_err(unreadable)?;
    Ok(if extension(&head).is_some() {
        Kind::Image { size }
    } else if let Some(format) = unsupported(&head) {
        Kind::Unsupported { size, format }
    } else {
        Kind::File { size }
    })
}

/// The file name extension of a PNG, JPEG, GIF, BMP or WebP image, judged
/// from its first bytes.
pub fn extension(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return Some("webp");
    }
    [
        (&b"\x89PNG\r\n\x1a\n"[..], "png"),
        (b"\xff\xd8\xff", "jpg"),
        (b"GIF87a", "gif"),
        (b"GIF89a", "gif"),
        (b"BM", "bmp"),
    ]
    .into_iter()
    .find_map(|(magic, extension)| head.starts_with(magic).then_some(extension))
}

/// HEIC, AVIF, TIFF, JPEG XL or SVG, judged from the first bytes.
fn unsupported(head: &[u8]) -> Option<&'static str> {
    if [&b"II*\0"[..], b"MM\0*", b"II+\0", b"MM\0+"]
        .iter()
        .any(|magic| head.starts_with(magic))
    {
        return Some("TIFF");
    }
    if head.starts_with(b"\xff\x0a") || head.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") {
        return Some("JPEG XL");
    }
    if head.get(4..8) == Some(b"ftyp") {
        // The major brand, and the compatible brands after the minor version.
        let end = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let major = head[8..].as_chunks::<4>().0.first();
        let compatible = head.get(16..end.min(head.len())).unwrap_or_default();
        let brands = || major.into_iter().chain(compatible.as_chunks::<4>().0);
        if brands().any(|brand| matches!(brand, b"avif" | b"avis")) {
            return Some("AVIF");
        }
        let heif = [
            b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1",
            b"msf1",
        ];
        if brands().any(|brand| heif.contains(&brand)) {
            return Some("HEIC");
        }
    }
    let text = head
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(head)
        .trim_ascii_start();
    let prologue = [&b"<?xml"[..], b"<!--", b"<!DOCTYPE svg"]
        .iter()
        .any(|start| text.starts_with(start));
    (text.starts_with(b"<svg") || prologue && text.windows(4).any(|window| window == b"<svg"))
        .then_some("SVG")
}

/// Settle image `id` in `tray` (a composer's, or a sent message's) with
/// what it was prepared as. One that cannot go as its message's choice has
/// it goes as its original file only when that message has Strip metadata
/// unticked; otherwise it leaves the tray.
/// Returns a note for the user.
pub fn settle(
    tray: &mut Vec<Attachment>,
    id: u64,
    result: Result<Ready, String>,
    strip_metadata: bool,
) -> Option<String> {
    let position = tray.iter().position(|attachment| attachment.id() == id)?;
    let Attachment::Image {
        name,
        source,
        prepared,
        ..
    } = &mut tray[position]
    else {
        return None;
    };
    let result = result.and_then(|ready| match (ready, &*source) {
        (Ready::Encoded(encoded), _) => match encoded.thumbnail {
            nuschl_core::ContentBlock::Image {
                width,
                height,
                rgba,
                ..
            } => crate::document::render_image(width, height, &rgba)
                .map(|thumbnail| Prepared::Inline(encoded.html, thumbnail))
                .ok_or_else(|| "the thumbnail is invalid".to_owned()),
            _ => Err("the thumbnail is missing".to_owned()),
        },
        (Ready::Original, Source::File { path, size, .. }) => Ok(Prepared::File(
            Offered::file(name.clone(), path.clone()),
            *size,
        )),
        (Ready::Original, Source::Bytes(_)) => Err("it has no file".to_owned()),
        // A pasted image is offered under its name with its format's extension.
        (Ready::Staged(path, size), source) => {
            let name = match source {
                Source::Bytes(bytes) => format!("{name}.{}", extension(bytes).unwrap_or("img")),
                Source::File { .. } => name.clone(),
            };
            let offered = Offered {
                name,
                path,
                staged: true,
            };
            Ok(Prepared::File(offered, size))
        }
    });
    match (result, &*source) {
        (Ok(ready), _) => {
            *prepared = Some(ready);
            None
        }
        (Err(reason), Source::File { path, size, .. }) if !strip_metadata => {
            let original = Offered::file(name.clone(), path.clone());
            *prepared = Some(Prepared::File(original, *size));
            Some(format!("{name} goes as a file: {reason}."))
        }
        (Err(reason), _) => {
            let note = format!("Could not attach {name}: {reason}.");
            tray.remove(position);
            Some(note)
        }
    }
}

/// Write `bytes` to `path`, in the staging folder, for the transfer worker to
/// offer. Copies that earlier runs left there over a day ago, after their
/// offers ended, go.
pub fn stage(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let folder = path.parent().ok_or("no staging folder")?;
    nuschl_session::layout::create_private_dir(folder)
        .and_then(|()| std::fs::write(path, bytes))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    let old = SystemTime::now() - crate::transfer::OFFER_LIFETIME - Duration::from_secs(3600);
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let modified = entry.metadata().and_then(|metadata| metadata.modified());
        if modified.is_ok_and(|modified| modified < old) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Read an attached image, refusing one that has grown past the limit.
pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("cannot read it: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read it: {error}"))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("it is larger than 8 MB".into());
    }
    Ok(bytes)
}

/// One line about the paths that could not be attached.
pub fn refusal(refused: &[(String, Refusal)]) -> Option<String> {
    let folders: Vec<&str> = refused
        .iter()
        .filter(|(_, refusal)| *refusal == Refusal::Folder)
        .map(|(name, _)| name.as_str())
        .collect();
    let mut parts = Vec::new();
    if !folders.is_empty() {
        parts.push(format!(
            "Folders cannot be attached ({}).",
            folders.join(", ")
        ));
    }
    for (name, refusal) in refused {
        if let Refusal::Unreadable(error) = refusal {
            parts.push(format!("Could not read {name}: {error}."));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// A name for a pasted image that no other attachment in the tray has.
pub fn pasted_name(tray: &[Attachment]) -> String {
    (1..)
        .map(|number| format!("Pasted image {number}"))
        .find(|name| tray.iter().all(|attachment| attachment.name() != name))
        .unwrap_or_default()
}

/// The messages that carry `caption` (Mumble HTML) and `images`. Stock Mumble
/// shows any number of images in a message, but the whole message must fit
/// the server's image limit: one image each keeps every image as large as
/// that allows. The caption goes with the first image when both fit the
/// server's limits, or on its own before them.
pub fn messages(
    caption: Option<String>,
    images: Vec<String>,
    image_limit: usize,
    message_limit: usize,
) -> Vec<String> {
    let mut images = images.into_iter();
    let mut messages = Vec::new();
    if let Some(caption) = caption {
        match images.next() {
            // Murmur also measures an image message without the images'
            // sources against its text limit (with some markup of its own).
            Some(first)
                if caption.len() + BREAK.len() + first.len() <= image_limit
                    && caption.len() + 64 <= message_limit =>
            {
                messages.push(format!("{caption}{BREAK}{first}"));
            }
            Some(first) => messages.extend([caption, first]),
            None => messages.push(caption),
        }
    }
    messages.extend(images);
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(size: usize) -> String {
        format!(
            "<img src=\"data:image/png;base64,{}\" />",
            "A".repeat(size - 36)
        )
    }

    #[test]
    fn images_go_one_per_message_with_the_text_in_the_first() {
        let images = vec![image(100_000), image(120_000), image(50)];
        let sent = messages(Some("Look &amp; see".into()), images.clone(), 131_072, 5000);
        assert_eq!(
            sent,
            [
                format!("Look &amp; see<br/>{}", images[0]),
                images[1].clone(),
                images[2].clone()
            ]
        );
        // Without text, just the images; without images, just the text.
        assert_eq!(messages(None, images.clone(), 131_072, 5000), images);
        assert_eq!(
            messages(Some("hi".into()), Vec::new(), 131_072, 5000),
            ["hi"]
        );
        assert!(messages(None, Vec::new(), 131_072, 5000).is_empty());
    }

    #[test]
    fn text_that_does_not_fit_beside_the_first_image_goes_before_it() {
        let images = vec![image(130_000), image(1000)];
        // Over the image limit together.
        let caption = "x".repeat(2000);
        let sent = messages(Some(caption.clone()), images.clone(), 131_072, 5000);
        assert_eq!(sent, [caption, images[0].clone(), images[1].clone()]);
        // Within the image limit, but too long for the text limit beside an image.
        let caption = "y".repeat(4990);
        let sent = messages(Some(caption.clone()), vec![image(1000)], 131_072, 5000);
        assert_eq!(sent, [caption, image(1000)]);
        // The caption room kept beside every image is enough for ordinary text.
        let first = image(131_072 - CAPTION_ROOM);
        let caption = "z".repeat(CAPTION_ROOM - BREAK.len());
        let sent = messages(Some(caption.clone()), vec![first.clone()], 131_072, 5000);
        assert_eq!(sent, [format!("{caption}{BREAK}{first}")]);
    }

    #[test]
    fn paths_go_as_images_or_files_judged_by_their_first_bytes() {
        let directory = std::env::temp_dir().join(format!("nuschl-attach-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = directory.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let png = write("shot", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        assert_eq!(check(&png), Ok(Kind::Image { size: 16 }));
        let jpeg = write("photo.jpg", b"\xff\xd8\xff\xe0");
        assert_eq!(check(&jpeg), Ok(Kind::Image { size: 4 }));
        let pdf = write("report.pdf", b"%PDF-1.7");
        assert_eq!(check(&pdf), Ok(Kind::File { size: 8 }));
        assert_eq!(check(&write("empty.txt", b"")), Ok(Kind::File { size: 0 }));
        let webp = write("photo.webp", b"RIFF\0\0\0\0WEBP");
        assert_eq!(check(&webp), Ok(Kind::Image { size: 12 }));
        // Images the content worker does not re-encode, whatever their name.
        for (format, bytes) in [
            ("HEIC", &b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"[..]),
            ("HEIC", b"\0\0\0\x18ftypmif1\0\0\0\0mif1miaf"),
            ("AVIF", b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf"),
            ("AVIF", b"\0\0\0\x18ftypmif1\0\0\0\0mif1avif"),
            ("TIFF", b"II*\0\x08\0\0\0"),
            ("TIFF", b"MM\0*\0\0\0\x08"),
            ("JPEG XL", b"\xff\x0a\0\0"),
            ("JPEG XL", b"\0\0\0\x0cJXL \r\n\x87\n"),
            ("SVG", b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
            (
                "SVG",
                b"\xef\xbb\xbf\n<?xml version='1.0'?>\n<!-- Editor -->\n<svg/>",
            ),
        ] {
            let size = bytes.len() as u64;
            assert_eq!(
                check(&write("photo", bytes)),
                Ok(Kind::Unsupported { size, format }),
                "{format}"
            );
        }
        // Videos and other XML go as files; the minor version is no brand.
        for bytes in [
            &b"\0\0\0\x18ftypisom\0\0\0\0isommp42"[..],
            b"\0\0\0\x10ftypisomavif",
            b"<?xml version='1.0'?><feed/>",
            b"<!DOCTYPE html><svg/>",
        ] {
            let size = bytes.len() as u64;
            assert_eq!(check(&write("other", bytes)), Ok(Kind::File { size }));
        }
        assert_eq!(check(&directory), Err(Refusal::Folder));
        assert!(matches!(
            check(&directory.join("missing.png")),
            Err(Refusal::Unreadable(_))
        ));
        // An image too large for the content worker goes to it all the same,
        // which refuses it (see `settle`).
        let large = write("large.png", b"\x89PNG\r\n\x1a\n");
        std::fs::File::options()
            .write(true)
            .open(&large)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        assert_eq!(
            check(&large),
            Ok(Kind::Image {
                size: MAX_IMAGE_BYTES + 1
            })
        );
        assert!(read(&large).is_err());
        assert_eq!(read(&png).unwrap().len(), 16);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    fn file(id: u64) -> Source {
        Source::File {
            path: PathBuf::from(format!("/photos/IMG_{id}.jpg")),
            size: 9_000_000,
            unsupported: None,
        }
    }

    fn photo(id: u64, source: Source) -> Attachment {
        Attachment::Image {
            id,
            name: format!("IMG_{id}.jpg"),
            source,
            prepared: None,
        }
    }

    fn encoded() -> Ready {
        Ready::Encoded(nuschl_ipc::EncodedImage {
            html: "<img />".into(),
            thumbnail: nuschl_core::ContentBlock::Image {
                width: 1,
                height: 1,
                rgba: "AAAA/w==".into(),
                source: None,
            },
        })
    }

    #[test]
    fn each_choice_has_its_plan() {
        use ImageQuality::{High, Low, Original};
        let choice = |strip_metadata, quality| Choice {
            strip_metadata,
            quality,
        };
        let pasted = Source::Bytes(Arc::new(b"\x89PNG".to_vec()));
        let heic = Source::File {
            path: "/photos/IMG_0042.HEIC".into(),
            size: 9,
            unsupported: Some("HEIC"),
        };
        for source in [&file(1), &pasted] {
            for strip in [true, false] {
                // Low and High are re-encoded; ticked or not, nothing else
                // goes.
                for quality in [Low, High] {
                    let planned = plan(source, choice(strip, quality), true, true);
                    assert_eq!(planned, Ok(Plan::Encode(quality)));
                }
                // Without the file transfer, Original goes as High.
                let planned = plan(source, choice(strip, Original), false, true);
                assert_eq!(planned, Ok(Plan::Encode(High)));
            }
            // Original: without its metadata, or as it is.
            assert_eq!(
                plan(source, choice(true, Original), true, true),
                Ok(Plan::Strip)
            );
            assert_eq!(
                plan(source, choice(false, Original), true, true),
                Ok(Plan::AsIs)
            );
            // A server without images still takes Original's file offer.
            assert_eq!(
                plan(source, choice(true, Original), true, false),
                Ok(Plan::Strip)
            );
            assert_eq!(
                plan(source, choice(true, High), true, false),
                Err("this server does not accept images".into())
            );
        }
        // A format the content worker does not read: as it is, or not at all.
        assert_eq!(
            plan(&heic, choice(false, Original), true, true),
            Ok(Plan::AsIs)
        );
        assert_eq!(
            plan(&heic, choice(true, Original), true, true),
            Err("HEIC metadata cannot be removed from the original".into())
        );
        assert_eq!(
            plan(&heic, choice(false, Low), true, true),
            Err("HEIC cannot be re-encoded".into())
        );
    }

    #[test]
    fn images_that_cannot_be_re_encoded_go_as_files_only_when_their_message_is_unticked() {
        let pdf = Attachment::File {
            id: 9,
            name: "report.pdf".into(),
            path: "/report.pdf".into(),
            size: 8,
        };
        let pasted = || Source::Bytes(Arc::new(b"\x89PNG".to_vec()));
        let failed = || Err("it is larger than 8 MB".to_owned());

        // Ticked: every failure, from a file or pasted, leaves the tray with a
        // note; the tray (or sent message) keeps no path to the original.
        let mut tray = vec![
            photo(1, file(1)),
            pdf,
            photo(2, pasted()),
            photo(3, file(3)),
        ];
        for id in [1, 2, 3] {
            let note = settle(&mut tray, id, failed(), true).unwrap();
            assert_eq!(
                note,
                format!("Could not attach IMG_{id}.jpg: it is larger than 8 MB.")
            );
        }
        assert!(matches!(tray.as_slice(), [Attachment::File { id: 9, .. }]));
        assert!(tray.iter().all(Attachment::ready));
        assert_eq!(settle(&mut tray, 1, failed(), true), None);

        // Unticked for its message: an image from a file goes as its
        // original; a pasted one has no file to go as.
        let mut tray = vec![photo(1, file(1)), photo(2, pasted())];
        let note = settle(&mut tray, 1, failed(), false).unwrap();
        assert_eq!(note, "IMG_1.jpg goes as a file: it is larger than 8 MB.");
        assert!(matches!(
            &tray[0],
            Attachment::Image { prepared: Some(Prepared::File(offered, 9_000_000)), .. }
                if offered.path == Path::new("/photos/IMG_1.jpg") && !offered.staged
        ));
        settle(&mut tray, 2, failed(), false).unwrap();
        assert_eq!(tray.len(), 1);

        // Re-encoded: the element, its size, and its thumbnail.
        let mut tray = vec![photo(4, file(4))];
        assert_eq!(settle(&mut tray, 4, Ok(encoded()), false), None);
        let Attachment::Image {
            prepared: Some(prepared @ Prepared::Inline(..)),
            ..
        } = &tray[0]
        else {
            panic!("not prepared");
        };
        assert_eq!(prepared.size(), 7);
    }

    #[test]
    fn originals_go_as_their_file_or_a_staged_copy() {
        let directory = std::env::temp_dir().join(format!("nuschl-offers-{}", std::process::id()));
        let original = directory.join("IMG_5.jpg");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&original, b"\xff\xd8\xff original bytes").unwrap();

        // Unticked, from a file: the file itself, byte for byte.
        let source = Source::File {
            path: original.clone(),
            size: 18,
            unsupported: None,
        };
        let mut tray = vec![photo(5, source)];
        assert_eq!(settle(&mut tray, 5, Ok(Ready::Original), false), None);
        let Some(Attachment::Image {
            prepared: Some(Prepared::File(offered, 18)),
            ..
        }) = tray.pop()
        else {
            panic!("not prepared");
        };
        assert_eq!(offered.path, original);
        assert_eq!(offered.name, "IMG_5.jpg");
        drop(offered);
        assert_eq!(
            std::fs::read(&original).unwrap(),
            b"\xff\xd8\xff original bytes"
        );

        // Pasted, unticked: a staged copy, byte for byte, offered under the
        // pasted name with its format's extension, deleted with the offer.
        let bytes = b"\x89PNG\r\n\x1a\n pasted".to_vec();
        let staged = directory.join("offers/1-6");
        stage(&staged, &bytes).unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), bytes);
        let mut tray = vec![Attachment::Image {
            id: 6,
            name: "Pasted image 1".into(),
            source: Source::Bytes(Arc::new(bytes)),
            prepared: None,
        }];
        let ready = Ready::Staged(staged.clone(), 16);
        assert_eq!(settle(&mut tray, 6, Ok(ready), false), None);
        let Some(Attachment::Image {
            prepared: Some(Prepared::File(offered, 16)),
            ..
        }) = tray.pop()
        else {
            panic!("not prepared");
        };
        assert_eq!(offered.name, "Pasted image 1.png");
        assert!(staged.exists());
        drop(offered);
        assert!(!staged.exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn refusals_name_the_paths_in_one_line() {
        let refused = [
            ("Photos".to_owned(), Refusal::Folder),
            (
                "b.png".to_owned(),
                Refusal::Unreadable("Permission denied".into()),
            ),
            ("Music".to_owned(), Refusal::Folder),
        ];
        assert_eq!(
            refusal(&refused).unwrap(),
            "Folders cannot be attached (Photos, Music). \
             Could not read b.png: Permission denied."
        );
        assert_eq!(refusal(&[]), None);
    }

    #[test]
    fn pasted_images_get_names_of_their_own() {
        let image = |name: &str| Attachment::Image {
            id: 0,
            name: name.into(),
            source: Source::Bytes(Arc::default()),
            prepared: None,
        };
        assert_eq!(pasted_name(&[]), "Pasted image 1");
        let tray = [image("Pasted image 2"), image("Pasted image 1")];
        assert_eq!(pasted_name(&tray), "Pasted image 3");
        assert_eq!(pasted_name(&tray[..1]), "Pasted image 1");
    }
}
