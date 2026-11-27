// SPDX-License-Identifier: EUPL-1.2
//! Adding music through the import service (docs/musicbot.md, "Adding
//! music"): a bounded blocking client, the requester pseudonyms, and the
//! owned poller that tells viewers when imports change.
//!
//! Replies are untrusted. Every reply is capped, parsed into typed values and
//! checked before anything of it reaches a viewer; failures become a fixed
//! [`Failure`] with a fixed public text.
use crate::{config, player::Changes, subsonic, tokens::Owner};
use hmac::{KeyInit, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{self, Read},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, Semaphore, watch},
    task::JoinHandle,
};

/// Actual blocking calls to the import service at once, the poller's included.
pub const MAX_IMPORT_CALLS: usize = 4;
pub static IMPORT_CALLS: Semaphore = Semaphore::const_new(MAX_IMPORT_CALLS);
/// End-to-end transport deadline of one call.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Largest reply read, JSON and covers alike.
pub const MAX_REPLY_BYTES: u64 = 1024 * 1024;
/// Results asked for per search.
const SEARCH_LIMIT: usize = 25;
/// Imports asked for per list.
const LIST_LIMIT: usize = 50;
/// The protocol's own bounds.
const MAX_ALBUMS: usize = 50;
const MAX_TRACKS: usize = 200;
const MAX_IMPORTS: usize = 50;
/// Longest title or artist name passed on, in bytes.
const MAX_TEXT: usize = 1024;
/// Largest track count accepted in a reply.
const MAX_COUNT: u32 = 100_000;
/// The requester the bot itself watches the import list as.
pub const BOT_REQUESTER: &str = "musicbot";
/// The secret behind the per-person pseudonyms, below the state directory.
pub const SECRET_FILE: &str = "import_requester.key";
const SECRET_BYTES: usize = 32;

/// Why a call failed, as viewers learn it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    RateLimited,
    RequesterLimit,
    TotalLimit,
    TooManyTracks,
    NotAvailable,
    Unavailable,
    /// The bot's own admission for import calls is full.
    Busy,
}

impl Failure {
    pub fn status(self) -> u16 {
        match self {
            Self::RateLimited => 429,
            Self::RequesterLimit | Self::TotalLimit => 409,
            Self::TooManyTracks => 422,
            Self::NotAvailable => 404,
            Self::Unavailable | Self::Busy => 503,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::RateLimited => "Too many searches at once. Try again in a few seconds.",
            Self::RequesterLimit => "You have added as much as you can for today. Try again later.",
            Self::TotalLimit => {
                "The library has taken in as much as it can for today. Try again later."
            }
            Self::TooManyTracks => "This album is too large to add.",
            Self::NotAvailable => "This album cannot be added.",
            Self::Unavailable => "Adding music is not available right now.",
            Self::Busy => "Too many requests at once. Try again in a moment.",
        }
    }

    /// The failure a service error reply stands for. Only the code and the
    /// limit's scope are read; anything else maps to [`Failure::Unavailable`].
    fn from_reply(body: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Reply {
            error: String,
            #[serde(default)]
            scope: Option<String>,
        }
        let Ok(reply) = serde_json::from_slice::<Reply>(body) else {
            return Self::Unavailable;
        };
        match (reply.error.as_str(), reply.scope.as_deref()) {
            ("rate_limited", _) => Self::RateLimited,
            ("limit_reached", Some("requester")) => Self::RequesterLimit,
            ("limit_reached", Some("total")) => Self::TotalLimit,
            ("too_many_tracks", _) => Self::TooManyTracks,
            ("not_available" | "not_found", _) => Self::NotAvailable,
            _ => Self::Unavailable,
        }
    }
}

/// Import service ids: exactly 16 lowercase hexadecimal characters.
pub fn valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Requesters: 1 to 64 characters of `[A-Za-z0-9._:-]`.
fn valid_requester(requester: &str) -> bool {
    (1..=64).contains(&requester.len())
        && requester
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn valid_text(text: &str) -> bool {
    text.len() <= MAX_TEXT && !text.chars().any(char::is_control)
}

fn valid_year(year: Option<i32>) -> bool {
    year.is_none_or(|year| (0..=9999).contains(&year))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseType {
    Album,
    Ep,
    Single,
    Compilation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Downloading,
    HandedOver,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    pub track_count: Option<u32>,
    pub release_type: ReleaseType,
    pub has_cover: bool,
}

impl Album {
    fn valid(&self) -> bool {
        valid_id(&self.id)
            && valid_text(&self.title)
            && valid_text(&self.artist)
            && valid_year(self.year)
            && self.track_count.is_none_or(|count| count <= MAX_COUNT)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlbumTrack {
    pub position: u32,
    pub title: String,
    pub artist: String,
    /// In seconds.
    pub duration: Option<u32>,
}

/// An album page: the album and its tracks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlbumDetails {
    #[serde(flatten)]
    pub album: Album,
    pub tracks: Vec<AlbumTrack>,
}

impl AlbumDetails {
    fn valid(&self) -> bool {
        self.album.valid()
            && self.tracks.len() <= MAX_TRACKS
            && self.tracks.iter().all(|track| {
                valid_text(&track.title)
                    && valid_text(&track.artist)
                    && track.position <= MAX_COUNT
                    && track.duration.is_none_or(|seconds| seconds <= 24 * 60 * 60)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Import {
    pub id: String,
    pub album_id: String,
    pub status: Status,
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    pub release_type: ReleaseType,
    pub has_cover: bool,
    pub tracks_total: u32,
    pub tracks_done: u32,
    pub tracks_failed: u32,
    /// Unix seconds.
    pub requested_at: u64,
    pub handed_over_at: Option<u64>,
    pub mine: bool,
}

impl Import {
    fn valid(&self) -> bool {
        valid_id(&self.id)
            && valid_id(&self.album_id)
            && valid_text(&self.title)
            && valid_text(&self.artist)
            && valid_year(self.year)
            && self.tracks_total <= MAX_COUNT
            && self.tracks_done <= self.tracks_total
            && self.tracks_failed <= self.tracks_done
    }

    fn active(&self) -> bool {
        matches!(self.status, Status::Queued | Status::Downloading)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub limit: u64,
    pub used: u64,
    pub next_free_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Budgets {
    pub requester: Budget,
    pub total: Budget,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Imports {
    pub imports: Vec<Import>,
    pub budget: Budgets,
}

/// The import service at one base URL with its key.
pub struct Client {
    /// `<url>api/integration/v1/`.
    base: String,
    authorization: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(url: &str, key: &str) -> Self {
        let separator = if url.ends_with('/') { "" } else { "/" };
        Self {
            base: format!("{url}{separator}api/integration/v1/"),
            authorization: format!("Bearer {key}"),
            // The library transport: no redirects, so the key never follows one.
            agent: subsonic::transport_agent(url, Some(TIMEOUT), TIMEOUT),
        }
    }

    fn get(
        &self,
        path: &str,
        requester: &str,
    ) -> Result<ureq::RequestBuilder<ureq::typestate::WithoutBody>, Failure> {
        if !valid_requester(requester) {
            return Err(Failure::Unavailable);
        }
        Ok(self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", &self.authorization)
            .query("requester", requester))
    }

    pub fn search(&self, requester: &str, query: &str) -> Result<Vec<Album>, Failure> {
        #[derive(Deserialize)]
        struct Reply {
            albums: Vec<Album>,
        }
        let response = self
            .get("search", requester)?
            .query("q", query)
            .query("limit", SEARCH_LIMIT.to_string())
            .call();
        let reply: Reply = json(response)?;
        if reply.albums.len() > MAX_ALBUMS || !reply.albums.iter().all(Album::valid) {
            return Err(Failure::Unavailable);
        }
        Ok(reply.albums)
    }

    pub fn album(&self, requester: &str, id: &str) -> Result<AlbumDetails, Failure> {
        if !valid_id(id) {
            return Err(Failure::NotAvailable);
        }
        let reply: AlbumDetails = json(self.get(&format!("albums/{id}"), requester)?.call())?;
        if !reply.valid() || reply.album.id != id {
            return Err(Failure::Unavailable);
        }
        Ok(reply)
    }

    /// The encoded cover; only the confined content worker reads it.
    pub fn cover(&self, requester: &str, id: &str) -> Result<Vec<u8>, Failure> {
        if !valid_id(id) {
            return Err(Failure::NotAvailable);
        }
        let mut response = self
            .get(&format!("covers/{id}"), requester)?
            .call()
            .map_err(unreachable)?;
        let body = read_capped(&mut response)?;
        if !response.status().is_success() {
            return Err(refused(response.status(), &body));
        }
        Ok(body)
    }

    pub fn imports(&self, requester: &str) -> Result<Imports, Failure> {
        let response = self
            .get("imports", requester)?
            .query("limit", LIST_LIMIT.to_string())
            .call();
        let reply: Imports = json(response)?;
        if reply.imports.len() > MAX_IMPORTS || !reply.imports.iter().all(Import::valid) {
            return Err(Failure::Unavailable);
        }
        Ok(reply)
    }

    /// Asks for an album; `true` with an import that was asked for already.
    pub fn request(&self, requester: &str, album_id: &str) -> Result<(Import, bool), Failure> {
        #[derive(Serialize)]
        struct Request<'a> {
            album_id: &'a str,
            requester: &'a str,
        }
        #[derive(Deserialize)]
        struct Reply {
            import: Import,
        }
        if !valid_id(album_id) {
            return Err(Failure::NotAvailable);
        }
        if !valid_requester(requester) {
            return Err(Failure::Unavailable);
        }
        let mut response = self
            .agent
            .post(format!("{}imports", self.base))
            .header("Authorization", &self.authorization)
            .send_json(Request {
                album_id,
                requester,
            })
            .map_err(unreachable)?;
        let existing = match response.status().as_u16() {
            202 => false,
            200 => true,
            _ => return Err(refused(response.status(), &read_capped(&mut response)?)),
        };
        let reply: Reply = parse(&read_capped(&mut response)?)?;
        if !reply.import.valid() || reply.import.album_id != album_id {
            return Err(Failure::Unavailable);
        }
        Ok((reply.import, existing))
    }
}

fn read_capped(response: &mut ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, Failure> {
    response
        .body_mut()
        .with_config()
        .limit(MAX_REPLY_BYTES)
        .read_to_vec()
        .map_err(unreachable)
}

fn unreachable(error: ureq::Error) -> Failure {
    tracing::warn!("import service: {error}");
    Failure::Unavailable
}

/// The failure an error reply stands for, logged with its status.
fn refused(status: ureq::http::StatusCode, body: &[u8]) -> Failure {
    let failure = Failure::from_reply(body);
    if failure == Failure::Unavailable {
        tracing::warn!("import service answered {status}");
    } else {
        tracing::debug!("import service answered {status}: {failure:?}");
    }
    failure
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Failure> {
    serde_json::from_slice(body).map_err(|error| {
        tracing::warn!("import service reply: {error}");
        Failure::Unavailable
    })
}

/// A capped JSON reply, or the failure its error reply stands for.
fn json<T: DeserializeOwned>(
    response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<T, Failure> {
    let mut response = response.map_err(unreachable)?;
    let body = read_capped(&mut response)?;
    if response.status().is_success() {
        parse(&body)
    } else {
        Err(refused(response.status(), &body))
    }
}

/// Stable per-person pseudonyms: HMAC-SHA256 under a secret kept in the state
/// directory, over the Mumble server and the person's certificate hash. They
/// name requesters to the import service and owners of personal playlists.
pub struct Pseudonyms {
    secret: [u8; SECRET_BYTES],
}

impl Pseudonyms {
    /// Reads the secret, or makes one on first use.
    pub fn load(state_directory: &Path) -> io::Result<Self> {
        let path = state_directory.join(SECRET_FILE);
        match crate::persist::open_private(&path) {
            Ok(file) => {
                let mut bytes = Vec::with_capacity(SECRET_BYTES);
                file.take(SECRET_BYTES as u64 + 1).read_to_end(&mut bytes)?;
                let secret = bytes.try_into().map_err(|_| {
                    io::Error::other(format!("{SECRET_FILE} must hold exactly 32 bytes"))
                })?;
                Ok(Self { secret })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut secret = [0; SECRET_BYTES];
                getrandom::fill(&mut secret).map_err(io::Error::other)?;
                crate::persist::write(&path, &secret)?;
                Ok(Self { secret })
            }
            Err(error) => Err(error),
        }
    }

    /// `"r"` and 32 hexadecimal characters. People without a certificate
    /// share one pseudonym per server.
    pub fn of(&self, owner: &Owner) -> String {
        derive(
            &self.secret,
            &owner.server,
            owner.certificate.as_deref().unwrap_or("anonymous"),
        )
    }
}

fn derive(secret: &[u8], server: &str, certificate: &str) -> String {
    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(secret).expect("HMAC takes keys of any length");
    mac.update(server.as_bytes());
    mac.update(&[0]);
    mac.update(certificate.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut pseudonym = String::with_capacity(33);
    pseudonym.push('r');
    for byte in &digest[..16] {
        pseudonym.push_str(&format!("{byte:02x}"));
    }
    pseudonym
}

/// What the bot holds for adding music.
pub struct Adding {
    pub client: Arc<Client>,
    /// Index of the library added albums arrive in.
    pub library: usize,
    /// Advances when the import list changes; viewers see it in the state.
    pub revision: AtomicU64,
    /// Wakes the poller early, after a request through the bot.
    wake: Notify,
}

impl Adding {
    /// Reads the key; fails startup when it is unusable.
    pub fn open(config: &config::Import, libraries: &[config::Library]) -> Result<Self, String> {
        let library = config
            .library_index(libraries)
            .ok_or("import.library names no [[subsonic]] library")?;
        let key = config.key()?;
        Ok(Self {
            client: Arc::new(Client::new(&config.url, &key)),
            library,
            revision: AtomicU64::new(0),
            wake: Notify::new(),
        })
    }

    /// After a successful request through the bot: the poller looks again now
    /// and raises the revision when it sees the change.
    pub fn requested(&self) {
        self.wake.notify_one();
    }
}

/// Poll intervals: while something is being added, and otherwise or after a failure.
const ACTIVE_INTERVAL: Duration = Duration::from_secs(3);
const IDLE_INTERVAL: Duration = Duration::from_secs(30);

/// What a list change is detected by.
type Fingerprint = Vec<(String, Status, u32)>;

fn fingerprint(imports: &Imports) -> Fingerprint {
    imports
        .imports
        .iter()
        .map(|import| (import.id.clone(), import.status, import.tracks_done))
        .collect()
}

/// Keep this handle for the whole runtime and call `shutdown().await` on exit.
#[must_use = "the import poller must be owned and shut down"]
pub struct Poller {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Poller {
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        if self.task.await.is_err() {
            tracing::warn!("import poller failed");
        }
    }
}

/// Starts the poller: once now, then every 3 s while an import is queued or
/// being added, every 30 s otherwise.
pub fn start(adding: Arc<Adding>, changes: Changes) -> Poller {
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut last: Option<Fingerprint> = None;
        loop {
            let interval = tokio::select! {
                biased;
                _ = stopped.changed() => return,
                interval = poll(&adding, &changes, &mut last) => interval,
            };
            tokio::select! {
                biased;
                _ = stopped.changed() => return,
                _ = adding.wake.notified() => {}
                _ = tokio::time::sleep(interval) => {}
            }
        }
    });
    Poller { stop, task }
}

/// One look at the list; returns how long to wait for the next.
async fn poll(adding: &Adding, changes: &Changes, last: &mut Option<Fingerprint>) -> Duration {
    // Waits for admission: the poller yields to nobody's deadline.
    let Ok(permit) = IMPORT_CALLS.acquire().await else {
        return IDLE_INTERVAL;
    };
    let client = adding.client.clone();
    let call = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        client.imports(BOT_REQUESTER)
    });
    match call.await {
        Ok(Ok(imports)) => {
            let current = fingerprint(&imports);
            if last.as_ref() != Some(&current) {
                adding.revision.fetch_add(1, Ordering::SeqCst);
                changes.notify();
                *last = Some(current);
            }
            if imports.imports.iter().any(Import::active) {
                ACTIVE_INTERVAL
            } else {
                IDLE_INTERVAL
            }
        }
        Ok(Err(_)) => IDLE_INTERVAL,
        Err(_) => {
            tracing::warn!("import list request failed");
            IDLE_INTERVAL
        }
    }
}

/// How titles and artist names are compared with the library's.
pub fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::tests::Server;
    use std::io::Write;

    #[test]
    fn ids_are_sixteen_lowercase_hex_characters() {
        assert!(valid_id("0123456789abcdef"));
        for id in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "0123456789ABCDEF",
            "0123456789abcdeg",
            "../../imports/aa",
            "0123456789abcd/f",
            "0123456789abcd%2",
        ] {
            assert!(!valid_id(id), "{id}");
        }
    }

    #[test]
    fn error_replies_map_to_failures() {
        for (body, failure) in [
            (
                r#"{"error":"limit_reached","scope":"requester"}"#,
                Failure::RequesterLimit,
            ),
            (
                r#"{"error":"limit_reached","scope":"total"}"#,
                Failure::TotalLimit,
            ),
            (
                r#"{"error":"limit_reached","scope":"elsewhere"}"#,
                Failure::Unavailable,
            ),
            (
                r#"{"error":"internal","message":"secret detail"}"#,
                Failure::Unavailable,
            ),
            ("<html>proxy error</html>", Failure::Unavailable),
        ] {
            assert_eq!(Failure::from_reply(body.as_bytes()), failure, "{body}");
        }
    }

    #[test]
    fn pseudonyms_are_stable_and_distinct() {
        let secret = [7; SECRET_BYTES];
        let alice = derive(&secret, "voice.example.org:64738", "aa11");
        assert_eq!(alice, derive(&secret, "voice.example.org:64738", "aa11"));
        assert_eq!(alice.len(), 33);
        assert!(alice.starts_with('r') && valid_requester(&alice));
        assert!(alice[1..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(alice, derive(&secret, "voice.example.org:64738", "bb22"));
        assert_ne!(alice, derive(&secret, "other.example.org:64738", "aa11"));
        assert_ne!(
            alice,
            derive(&[8; SECRET_BYTES], "voice.example.org:64738", "aa11")
        );
        // The separator keeps server and certificate apart.
        assert_ne!(derive(&secret, "ab", "c"), derive(&secret, "a", "bc"));
        assert_ne!(alice, BOT_REQUESTER);
    }

    #[test]
    fn people_without_certificates_share_one_pseudonym_per_server() {
        let pseudonyms = Pseudonyms {
            secret: [1; SECRET_BYTES],
        };
        let owner = |server: &str, name: &str, certificate: Option<&str>| Owner {
            server: server.into(),
            name: name.into(),
            certificate: certificate.map(Into::into),
        };
        let a = pseudonyms.of(&owner("one:1", "alice", None));
        assert_eq!(a, pseudonyms.of(&owner("one:1", "bob", None)));
        assert_ne!(a, pseudonyms.of(&owner("two:1", "alice", None)));
        assert_ne!(a, pseudonyms.of(&owner("one:1", "alice", Some("aa11"))));
        assert_eq!(
            pseudonyms.of(&owner("one:1", "alice", Some("aa11"))),
            pseudonyms.of(&owner("one:1", "renamed", Some("aa11")))
        );
    }

    #[test]
    fn the_secret_is_made_once_and_kept_private() {
        let directory = tempfile::tempdir().unwrap();
        let first = Pseudonyms::load(directory.path()).unwrap();
        let again = Pseudonyms::load(directory.path()).unwrap();
        assert_eq!(first.secret, again.secret);
        let path = directory.path().join(SECRET_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert!(Pseudonyms::load(directory.path()).is_err());
    }

    fn album(id: &str) -> Album {
        Album {
            id: id.into(),
            title: "Title".into(),
            artist: "Artist".into(),
            year: Some(2001),
            track_count: Some(10),
            release_type: ReleaseType::Album,
            has_cover: true,
        }
    }

    fn import(status: Status, done: u32) -> Import {
        Import {
            id: "00000000000000aa".into(),
            album_id: "00000000000000bb".into(),
            status,
            title: "Title".into(),
            artist: "Artist".into(),
            year: None,
            release_type: ReleaseType::Ep,
            has_cover: false,
            tracks_total: 4,
            tracks_done: done,
            tracks_failed: 0,
            requested_at: 1,
            handed_over_at: None,
            mine: false,
        }
    }

    #[test]
    fn replies_are_checked() {
        assert!(album("0123456789abcdef").valid());
        assert!(!album("0123456789ABCDEF").valid());
        let mut long = album("0123456789abcdef");
        long.title = "x".repeat(MAX_TEXT + 1);
        assert!(!long.valid());
        let mut control = album("0123456789abcdef");
        control.artist = "a\u{1b}[2J".into();
        assert!(!control.valid());
        let mut year = album("0123456789abcdef");
        year.year = Some(-5);
        assert!(!year.valid());
        assert!(import(Status::Downloading, 2).valid());
        assert!(!import(Status::Downloading, 5).valid());
        let mut failed = import(Status::HandedOver, 2);
        failed.tracks_failed = 3;
        assert!(!failed.valid());
        let parsed: Result<Album, _> = serde_json::from_str(
            r#"{"id":"0123456789abcdef","title":"T","artist":"A","year":null,
                "track_count":null,"release_type":"mixtape","has_cover":false}"#,
        );
        assert!(parsed.is_err());
        let details: AlbumDetails = serde_json::from_str(
            r#"{"id":"0123456789abcdef","title":"T","artist":"A","year":1999,
                "track_count":1,"release_type":"single","has_cover":false,
                "tracks":[{"position":1,"title":"T","artist":"A","duration":null}]}"#,
        )
        .unwrap();
        assert!(details.valid());
        let mut many = details.clone();
        many.tracks = vec![details.tracks[0].clone(); MAX_TRACKS + 1];
        assert!(!many.valid());
    }

    #[test]
    fn names_compare_without_case_or_spacing() {
        assert_eq!(normalized("  The   Album "), normalized("the album"));
        assert_ne!(normalized("The Album"), normalized("The Albums"));
    }

    /// The shared fixture server answering every request with `reply`.
    fn serve(reply: Vec<u8>) -> Server {
        Server::new(move |_, socket| {
            // A client that stopped reading at its cap closes early.
            let _ = socket.write_all(&reply);
        })
    }

    fn head(server: &Server) -> String {
        server.requests().pop().expect("one request")
    }

    fn response(status: &str, kind: &str, body: &[u8]) -> Vec<u8> {
        let mut reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        reply.extend_from_slice(body);
        reply
    }

    #[tokio::test]
    async fn the_poller_counts_each_change_as_the_bot() {
        use std::sync::atomic::AtomicUsize;
        let step = Arc::new(AtomicUsize::new(0));
        let server = Server::new({
            let step = step.clone();
            move |_, socket| {
                let (status, done) = match step.load(Ordering::SeqCst) {
                    0 => ("queued", 0),
                    1 | 2 => ("downloading", 1),
                    3 => {
                        let _ = socket.write_all(&response(
                            "500 Internal Server Error",
                            "text/plain",
                            b"",
                        ));
                        return;
                    }
                    _ => ("handed_over", 2),
                };
                let body = format!(
                    r#"{{"imports":[{{"id":"00000000000000aa","album_id":"0123456789abcdef","status":"{status}","title":"T","artist":"A","year":null,"release_type":"album","has_cover":false,"tracks_total":2,"tracks_done":{done},"tracks_failed":0,"requested_at":1,"handed_over_at":null,"mine":false}}],"budget":{{"requester":{{"limit":1,"used":0,"next_free_at":null}},"total":{{"limit":1,"used":0,"next_free_at":null}}}}}}"#
                );
                let _ = socket.write_all(&response("200 OK", "application/json", body.as_bytes()));
            }
        });
        let adding = Adding {
            client: Arc::new(Client::new(&server.url, "key")),
            library: 0,
            revision: AtomicU64::new(0),
            wake: Notify::new(),
        };
        let (changes, mut last) = (Changes::default(), None);
        let mut seen = Vec::new();
        for state in 0..5 {
            step.store(state, Ordering::SeqCst);
            let interval = poll(&adding, &changes, &mut last).await;
            seen.push((adding.revision.load(Ordering::SeqCst), interval));
        }
        // Queued and the first progress count; a repeat and a failure do not.
        assert_eq!(
            seen,
            [
                (1, ACTIVE_INTERVAL),
                (2, ACTIVE_INTERVAL),
                (2, ACTIVE_INTERVAL),
                (2, IDLE_INTERVAL),
                (3, IDLE_INTERVAL),
            ]
        );
        assert!(server.requests().iter().all(|head| {
            head.starts_with(&format!(
                "GET /api/integration/v1/imports?requester={BOT_REQUESTER}&"
            ))
        }));
    }

    #[test]
    fn requests_carry_the_key_and_requester() {
        let body = br#"{"albums":[{"id":"0123456789abcdef","title":"T","artist":"A","year":2000,"track_count":3,"release_type":"album","has_cover":true}]}"#;
        let server = serve(response("200 OK", "application/json", body));
        let client = Client::new(&server.url, "the-key");
        let albums = client.search("r0011", "some album").unwrap();
        assert_eq!(
            albums,
            vec![Album {
                year: Some(2000),
                track_count: Some(3),
                title: "T".into(),
                artist: "A".into(),
                ..album("0123456789abcdef")
            }]
        );
        let head = head(&server);
        assert!(
            head.starts_with(
                "GET /api/integration/v1/search?requester=r0011&q=some%20album&limit=25 "
            ),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer the-key\r\n"),
            "{head}"
        );
    }

    #[test]
    fn replies_over_one_mebibyte_are_refused() {
        let mut body = br#"{"albums":[],"padding":""#.to_vec();
        body.resize(MAX_REPLY_BYTES as usize + 1, b'x');
        body.extend_from_slice(br#""}"#);
        let server = serve(response("200 OK", "application/json", &body));
        let client = Client::new(&format!("{}/", server.url), "key");
        assert_eq!(client.search("r1", "q"), Err(Failure::Unavailable));
        let mut cover = vec![0xff; MAX_REPLY_BYTES as usize + 1];
        cover[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        let server = serve(response("200 OK", "image/jpeg", &cover));
        assert_eq!(
            Client::new(&server.url, "key").cover("r1", "0123456789abcdef"),
            Err(Failure::Unavailable)
        );
    }

    #[test]
    fn redirects_are_not_followed() {
        let server = serve(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        assert_eq!(
            Client::new(&server.url, "key").imports(BOT_REQUESTER),
            Err(Failure::Unavailable)
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn service_errors_and_bad_ids_stay_fixed() {
        let server = serve(response(
            "429 Too Many Requests",
            "application/json",
            br#"{"error":"rate_limited","retry_after":2}"#,
        ));
        assert_eq!(
            Client::new(&server.url, "key").search("r1", "q"),
            Err(Failure::RateLimited)
        );
        // Rejected before any connection: nothing listens on this client's address.
        let offline = Client::new("http://127.0.0.1:9/", "key");
        assert_eq!(
            offline.album("r1", "../imports"),
            Err(Failure::NotAvailable)
        );
        assert_eq!(
            offline.cover("r1", "ABCDEF0123456789"),
            Err(Failure::NotAvailable)
        );
        assert_eq!(offline.request("r1", "x"), Err(Failure::NotAvailable));
        assert_eq!(
            offline.search("bad requester", "q"),
            Err(Failure::Unavailable)
        );
    }

    #[test]
    fn a_new_request_and_an_existing_one_differ() {
        let import =
            serde_json::to_vec(&serde_json::json!({ "import": import(Status::Queued, 0) }))
                .unwrap();
        for (status, existing) in [("202 Accepted", false), ("200 OK", true)] {
            let server = serve(response(status, "application/json", &import));
            let (reply, seen) = Client::new(&server.url, "key")
                .request("r1", "00000000000000bb")
                .unwrap();
            assert_eq!((reply.status, seen), (Status::Queued, existing));
            let head = head(&server);
            assert!(
                head.starts_with("POST /api/integration/v1/imports "),
                "{head}"
            );
        }
        // An import for another album than asked for is a malformed reply.
        let server = serve(response("202 Accepted", "application/json", &import));
        assert_eq!(
            Client::new(&server.url, "key").request("r1", "00000000000000cc"),
            Err(Failure::Unavailable)
        );
    }
}
