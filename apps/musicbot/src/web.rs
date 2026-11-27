// SPDX-License-Identifier: EUPL-1.2
//! The web page: a Svelte app (apps/musicbot/web) served below `/u/<token>/`,
//! a JSON API for it, and a stream of state changes (Server-Sent Events) so
//! that every viewer sees what anybody changes. All the app's addresses are
//! relative to the link, so it also works behind a proxy under a path. The
//! contract is described in docs/musicbot.md ("Web API").
use crate::{
    Bot,
    covers::{CoverKey, IMPORT_NAMESPACE, ImageKey},
    import,
    player::{
        self as playback, BrowserSubscription, Curve, DECKS, DeckSong, DjError, Ducking, Listen,
        MAX_DUCK_HOLD,
    },
    subsonic::{
        self, Album, AlbumList, Artist, Genres, Library, LibraryIdentity, Lyrics, MAX_GENRE_NAME,
        SongDetails, SonicMatch, Track,
    },
    tokens::Owner,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{
        Html, IntoResponse, Redirect, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, atomic::Ordering},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{OwnedMutexGuard, Semaphore, SemaphorePermit, mpsc};
use uuid::Uuid;

mod playlists;

/// Searches longer than this are cut.
const MAX_QUERY: usize = 200;
/// How long a request waits for a library.
const LIBRARY_TIMEOUT: Duration = subsonic::API_TIMEOUT;
/// Count actual blocking calls, not callers still waiting for their replies.
const MAX_LIBRARY_CALLS: usize = 8;
static LIBRARY_CALLS: Semaphore = Semaphore::const_new(MAX_LIBRARY_CALLS);
/// A body owns its slot until the SSE stream ends or is dropped. No waiters.
const MAX_SSE_WATCHERS: usize = 32;
static SSE_WATCHERS: Semaphore = Semaphore::const_new(MAX_SSE_WATCHERS);
/// One worker run and its process exit, not HTTP lifetime.
const MAX_COVER_JOBS: usize = 2;
static COVER_JOBS: Semaphore = Semaphore::const_new(MAX_COVER_JOBS);
/// Encoded covers fetched at once. Cover fetches have their own admission, so
/// they leave [`LIBRARY_CALLS`] to browsing and run while worker jobs are busy.
const MAX_COVER_FETCHES: usize = 4;
static COVER_FETCHES: Semaphore = Semaphore::const_new(MAX_COVER_FETCHES);
/// Uncached covers that may wait for a fetch or a job at once (a page of
/// albums and some); more are refused at once.
const MAX_COVER_WAITERS: usize = 64;
static COVER_WAITERS: Semaphore = Semaphore::const_new(MAX_COVER_WAITERS);
/// How long an uncached cover waits for a fetch, and then for a job, before
/// it is refused.
const COVER_WAIT: Duration = Duration::from_secs(10);
/// How long a request waits for its worker job; the content worker's own
/// exchange and exit deadlines are shorter.
const COVER_JOB_TIMEOUT: Duration = Duration::from_secs(10);
/// Images being made, each with the lock its maker holds, so byte-identical
/// covers that arrive together wait for one worker job.
static MAKING: std::sync::Mutex<BTreeMap<ImageKey, Arc<tokio::sync::Mutex<()>>>> =
    std::sync::Mutex::new(BTreeMap::new());
/// Live bodies have their own admission, independent of library metadata calls.
const MAX_LISTEN_STREAMS: usize = 16;
static LISTEN_STREAMS: Semaphore = Semaphore::const_new(MAX_LISTEN_STREAMS);
/// Headphone cue streams of DJ decks, admitted beside the room's listeners.
const MAX_CUE_STREAMS: usize = 4;
static CUE_STREAMS: Semaphore = Semaphore::const_new(MAX_CUE_STREAMS);
/// A listening stream with nothing to send (a paused or waiting queue) sends
/// this one-byte record at least this often, so proxies with read timeouts
/// (nginx defaults to 60 s) keep it open.
const LISTEN_KEEPALIVE: Duration = Duration::from_secs(15);
const KEEPALIVE_RECORD: &[u8] = &[2];
/// Albums per page of an album list.
const PAGE_SIZE: u32 = 48;
/// Songs per page of a genre's songs.
const GENRE_SONGS_PAGE: u32 = 50;
/// The last page number a paged list asks for.
const MAX_PAGE: u32 = 10_000;
/// Changes that come in a burst are sent as one state.
const COALESCE: Duration = Duration::from_millis(50);
/// A viewer receives at most one state event in this time, so controls moved
/// thirty times a second (DJ faders) cost at most ten snapshots a second.
const MIN_EVENT_GAP: Duration = Duration::from_millis(100);
const INVALID_LINK: &str =
    "This link is not valid (any more). Send !web to the music bot in Mumble for a new one.";
/// Remembers the browser's own link, so token-free shared URLs can open it.
/// Over HTTPS the name carries the `__Host-` or `__Secure-` prefix.
const LINK_COOKIE: &str = "musicbot_link";
const LINK_COOKIE_MAX_AGE: u64 = 90 * 24 * 60 * 60;

pub fn router(bot: Arc<Bot>) -> Router {
    let links = Router::new()
        .route("/", get(root))
        .route("/u/{token}", get(slash))
        .route("/u/{token}/", get(remember))
        .route("/u/{token}/assets/{file}", get(asset))
        .route("/u/{token}/licenses", get(licenses))
        .route("/u/{token}/art/{library}/{id}", get(art))
        .route("/u/{token}/import-art/{id}", get(import_art))
        .route("/u/{token}/api/state", get(state))
        .route("/u/{token}/api/history", get(history))
        .route("/u/{token}/api/events", get(events))
        .route("/u/{token}/api/listen", get(listen))
        .route("/u/{token}/api/cue", get(cue))
        .route("/u/{token}/api/dj", post(dj))
        .route("/u/{token}/api/spectrum", get(spectrum))
        .route("/u/{token}/api/spectrum/overview", get(spectrum_overview))
        .route("/u/{token}/api/command", post(command))
        .route("/u/{token}/api/rating", get(rating).post(set_rating))
        .route("/u/{token}/api/albums", get(albums))
        .route("/u/{token}/api/album", get(album))
        .route("/u/{token}/api/artists", get(artists))
        .route("/u/{token}/api/artist", get(artist))
        .route("/u/{token}/api/genres", get(genres))
        .route("/u/{token}/api/genre/albums", get(genre_albums))
        .route("/u/{token}/api/genre/songs", get(genre_songs))
        .route("/u/{token}/api/playlists", get(playlists::list))
        .route("/u/{token}/api/playlist", get(playlists::detail))
        .route("/u/{token}/api/playlists/create", post(playlists::create))
        .route("/u/{token}/api/playlists/rename", post(playlists::rename))
        .route("/u/{token}/api/playlists/share", post(playlists::share))
        .route("/u/{token}/api/playlists/delete", post(playlists::delete))
        .route("/u/{token}/api/playlists/add", post(playlists::add))
        .route("/u/{token}/api/playlists/remove", post(playlists::remove))
        .route(
            "/u/{token}/api/favorites",
            get(playlists::favorite).post(playlists::set_favorite),
        )
        .route("/u/{token}/api/search", get(search))
        .route("/u/{token}/api/suggest", get(suggest))
        .route("/u/{token}/api/song", get(song))
        .route("/u/{token}/api/similar", get(similar))
        .route("/u/{token}/api/lyrics", get(lyrics))
        .route("/u/{token}/api/path", get(sonic_path))
        .route("/u/{token}/api/forget", post(forget))
        .route("/u/{token}/api/import/search", get(import_search))
        .route("/u/{token}/api/import/album", get(import_album))
        .route(
            "/u/{token}/api/import/requests",
            get(import_requests).post(import_request),
        )
        .with_state(bot.clone());
    // The outer router's layers run before the inner routing, so the page's
    // token-free requests can reach the per-link handlers. The header layer
    // is outermost so refused page requests get the same headers.
    Router::new()
        .fallback_service(links)
        .layer(axum::middleware::map_request_with_state(bot, page_request))
        .layer(axum::middleware::map_response(headers))
}

/// What the public address means for the page: its origin and how the
/// remembered link is stored.
struct Site {
    /// `scheme://host[:port]`, lower case, without a default port.
    origin: Option<String>,
    cookie: &'static str,
    path: String,
    secure: bool,
}

impl Site {
    fn new(public_url: &str) -> Self {
        let uri = public_url.parse::<axum::http::Uri>().ok();
        let secure = uri
            .as_ref()
            .and_then(|uri| uri.scheme_str())
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"));
        let path = uri
            .as_ref()
            .map(|uri| uri.path().trim_end_matches('/').to_owned() + "/")
            .filter(|path| {
                path.bytes()
                    .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b';' | b','))
            })
            .unwrap_or_else(|| "/".to_owned());
        // `__Host-` needs Secure and `Path=/`; `__Secure-` needs Secure.
        let cookie = match (secure, path.as_str()) {
            (true, "/") => "__Host-musicbot_link",
            (true, _) => "__Secure-musicbot_link",
            (false, _) => LINK_COOKIE,
        };
        Self {
            origin: uri.as_ref().and_then(origin_of),
            cookie,
            path,
            secure,
        }
    }
}

/// The normalized origin of an absolute URI, or `None` for anything else.
fn origin_of(uri: &axum::http::Uri) -> Option<String> {
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let host = uri.host()?.to_ascii_lowercase();
    let default = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    Some(match uri.port_u16() {
        Some(port) if port != default => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    })
}

/// The page runs at the token-free root and authenticates with the browser's
/// remembered link; the address bar never shows the key. Its relative requests
/// become the per-link routes. Without a usable cookie they name no link and
/// fail like an unknown one. Explicit `/u/<token>/` paths never use the cookie.
///
/// The cookie is sent by the browser on its own, so page requests must come
/// from the page itself: a `Sec-Fetch-Site` other than `same-origin` (or
/// `none`, typed by the user) and an `Origin` other than the public one are
/// refused. `SameSite=Lax` alone compares sites, not origins.
async fn page_request(
    State(bot): State<Arc<Bot>>,
    mut request: axum::extract::Request,
) -> Result<axum::extract::Request, ApiError> {
    let path = request.uri().path();
    let page_path = ["/api/", "/art/", "/import-art/", "/assets/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || path == "/licenses";
    if !page_path {
        return Ok(request);
    }
    let site = Site::new(&bot.config.web.public_url);
    if !same_origin(request.headers(), &site) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "requests from other sites cannot use this browser's link".to_owned(),
        ));
    }
    let token = remembered_link(request.headers(), &site)
        .flatten()
        .unwrap_or(Uuid::nil());
    let rewritten = match request.uri().query() {
        Some(query) => format!("/u/{token}{path}?{query}"),
        None => format!("/u/{token}{path}"),
    };
    if let Ok(uri) = rewritten.parse() {
        *request.uri_mut() = uri;
    }
    Ok(request)
}

fn same_origin(headers: &HeaderMap, site: &Site) -> bool {
    let fetch_site = headers
        .get_all(HeaderName::from_static("sec-fetch-site"))
        .iter()
        .all(|value| matches!(value.as_bytes(), b"same-origin" | b"none"));
    let origin = headers.get_all(header::ORIGIN).iter().all(|value| {
        let origin = value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<axum::http::Uri>().ok())
            .and_then(|uri| origin_of(&uri));
        origin.is_some() && origin == site.origin
    });
    fetch_site && origin
}

/// The link is a secret: no referrer, nothing from elsewhere, and nothing
/// cached except what a handler allows (cover art, the app's hashed files).
/// Images come from this origin only: the covers, which are PNGs the content
/// worker made.
async fn headers(mut response: Response) -> Response {
    let set = [
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
             connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
        (HeaderName::from_static("x-robots-tag"), "noindex"),
    ];
    let headers = response.headers_mut();
    for (name, value) in set {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// A failed API request: its status and `{"error": …}`.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        (self.0, Json(Body { error: self.1 })).into_response()
    }
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

fn owner(bot: &Bot, token: &str) -> Result<Owner, ApiError> {
    Uuid::parse_str(token)
        .ok()
        .and_then(|token| bot.tokens.owner(&token))
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, INVALID_LINK.to_owned()))
}

/// How an admitted blocking call ended without a result.
enum CallFailure<E> {
    /// The admission was full; nothing ran.
    Busy,
    Failed(E),
    Crashed,
    TimedOut,
}

/// Runs an admitted blocking call. Dropping or timing out its caller must not
/// release capacity while the real transport is still doing work.
async fn admitted_call<T: Send + 'static, E: Send + 'static>(
    admission: &'static Semaphore,
    timeout: Duration,
    call: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, CallFailure<E>> {
    let permit = admission.try_acquire().map_err(|_| CallFailure::Busy)?;
    blocking_call(timeout, move || {
        let _permit = permit;
        call()
    })
    .await
}

/// Runs a blocking call; whatever `call` owns, an admission permit included,
/// stays held until the call returns, also after its caller stopped waiting.
async fn blocking_call<T: Send + 'static, E: Send + 'static>(
    timeout: Duration,
    call: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, CallFailure<E>> {
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(call)).await {
        Ok(Ok(result)) => result.map_err(CallFailure::Failed),
        Ok(Err(_)) => Err(CallFailure::Crashed),
        Err(_) => Err(CallFailure::TimedOut),
    }
}

/// The library's redacted error for an admitted call that ended without a result.
fn library_failure(failure: CallFailure<subsonic::Error>) -> ApiError {
    match failure {
        CallFailure::Busy => ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the music library is busy".to_owned(),
        ),
        CallFailure::Failed(error) => ApiError(StatusCode::BAD_GATEWAY, error.public_message()),
        CallFailure::Crashed => ApiError(
            StatusCode::BAD_GATEWAY,
            "the music library request failed".to_owned(),
        ),
        CallFailure::TimedOut => ApiError(
            StatusCode::BAD_GATEWAY,
            "the library did not answer in time".to_owned(),
        ),
    }
}

/// An admitted library call with the library's redacted errors.
async fn blocking_library_call<T: Send + 'static>(
    admission: &'static Semaphore,
    timeout: Duration,
    call: impl FnOnce() -> Result<T, subsonic::Error> + Send + 'static,
) -> Result<T, ApiError> {
    admitted_call(admission, timeout, call)
        .await
        .map_err(library_failure)
}

fn known_library(bot: &Bot, library: usize) -> Result<(), ApiError> {
    if library < bot.libraries.len() {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "no such library".to_owned(),
        ))
    }
}

/// Runs a blocking library call with shared process-wide admission.
async fn ask<T: Send + 'static>(
    bot: &Bot,
    library: usize,
    call: impl FnOnce(&Library) -> Result<T, subsonic::Error> + Send + 'static,
) -> Result<T, ApiError> {
    known_library(bot, library)?;
    let libraries = bot.libraries.clone();
    blocking_library_call(&LIBRARY_CALLS, LIBRARY_TIMEOUT, move || {
        call(&libraries[library])
    })
    .await
}

/// How a checked library call failed: the library's error, or a refusal the
/// call found while it ran, for example after re-reading what it changes.
enum Checked {
    Library(subsonic::Error),
    Refused(ApiError),
}

impl From<subsonic::Error> for Checked {
    fn from(error: subsonic::Error) -> Self {
        Self::Library(error)
    }
}

/// [`ask`] for calls that may refuse with their own status.
async fn ask_checked<T: Send + 'static>(
    bot: &Bot,
    library: usize,
    call: impl FnOnce(&Library) -> Result<T, Checked> + Send + 'static,
) -> Result<T, ApiError> {
    known_library(bot, library)?;
    let libraries = bot.libraries.clone();
    admitted_call(&LIBRARY_CALLS, LIBRARY_TIMEOUT, move || {
        call(&libraries[library])
    })
    .await
    .map_err(|failure| match failure {
        CallFailure::Failed(Checked::Refused(error)) => error,
        CallFailure::Failed(Checked::Library(error)) => library_failure(CallFailure::Failed(error)),
        CallFailure::Busy => library_failure(CallFailure::Busy),
        CallFailure::Crashed => library_failure(CallFailure::Crashed),
        CallFailure::TimedOut => library_failure(CallFailure::TimedOut),
    })
}

/// The bot's public address: the page for a browser that opened its own link,
/// otherwise how to get one. Shared views keep their URL fragment.
async fn root(State(bot): State<Arc<Bot>>, headers: HeaderMap) -> Response {
    let site = Site::new(&bot.config.web.public_url);
    match remembered_link(&headers, &site) {
        Some(Some(token)) if bot.tokens.is_valid(&token) => app_page(&bot).await,
        remembered => {
            let mut response = Html(
                "<!DOCTYPE html><meta charset=\"utf-8\"><title>Music</title>\
                 <p>Send <code>!web</code> to the music bot in Mumble to get your link to this page.\
                 <p>Open it once in this browser; afterwards shared links work here as well.",
            )
            .into_response();
            if remembered.is_some() {
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, link_cookie(&site, None));
            }
            response
        }
    }
}

/// `None` without the cookie, `Some(None)` for a malformed value.
fn remembered_link(headers: &HeaderMap, site: &Site) -> Option<Option<Uuid>> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == site.cookie).then(|| Uuid::parse_str(value).ok())
        })
}

/// Remembers `token` (or forgets the link) below the configured public path.
fn link_cookie(site: &Site, token: Option<&Uuid>) -> HeaderValue {
    let secure = if site.secure { "; Secure" } else { "" };
    let (value, max_age) = match token {
        Some(token) => (token.to_string(), LINK_COOKIE_MAX_AGE),
        None => (String::new(), 0),
    };
    HeaderValue::from_str(&format!(
        "{}={value}; Path={}; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}",
        site.cookie, site.path
    ))
    .expect("cookie built from a UUID and a checked path")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Forget {}

/// Forgets the link remembered in this browser; the link itself stays valid.
/// JSON only, like the other POSTs.
async fn forget(State(bot): State<Arc<Bot>>, Json(Forget {}): Json<Forget>) -> Response {
    let site = Site::new(&bot.config.web.public_url);
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, link_cookie(&site, None))],
    )
        .into_response()
}

async fn slash(Path(token): Path<String>) -> Result<Redirect, ApiError> {
    // Path parameters are URL-decoded. Only a canonical UUID may become a
    // relative redirect, so encoded slashes cannot produce another origin.
    let token = Uuid::parse_str(&token)
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, INVALID_LINK.into()))?;
    // Keep this relative: deployments may mount the bot below a proxy prefix.
    Ok(Redirect::permanent(&format!("{token}/")))
}

fn assets(bot: &Bot) -> Result<&PathBuf, ApiError> {
    bot.web_assets.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "The web page was not installed: start the bot with --web-assets.".to_owned(),
        )
    })
}

/// The link from `!web`: remember it in this browser, then show the page at the
/// token-free root so the address bar can be copied safely.
async fn remember(State(bot): State<Arc<Bot>>, Path(token): Path<String>) -> Response {
    let Ok(token) = Uuid::parse_str(&token)
        .map_err(drop)
        .and_then(|token| bot.tokens.is_valid(&token).then_some(token).ok_or(()))
    else {
        let page = format!(
            "<!DOCTYPE html><meta charset=\"utf-8\"><title>Unknown link</title><p>{INVALID_LINK}"
        );
        return (StatusCode::NOT_FOUND, Html(page)).into_response();
    };
    // Relative, like `slash`: the bot may be mounted below a proxy prefix.
    let mut response = Redirect::to("../../").into_response();
    let site = Site::new(&bot.config.web.public_url);
    response
        .headers_mut()
        .insert(header::SET_COOKIE, link_cookie(&site, Some(&token)));
    response
}

async fn app_page(bot: &Bot) -> Response {
    let directory = match assets(bot) {
        Ok(directory) => directory,
        Err(error) => return error.into_response(),
    };
    match tokio::fs::read_to_string(directory.join("index.html")).await {
        Ok(page) => Html(page).into_response(),
        Err(error) => {
            tracing::error!("web assets: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The web page is missing.",
            )
                .into_response()
        }
    }
}

/// Only the installed notice aggregate is served; no request selects a path.
async fn licenses(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    use tokio::io::AsyncReadExt;

    owner(&bot, &token)?;
    let missing = || {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "The licence notices were not installed.".to_owned(),
        )
    };
    let file = tokio::fs::File::open(assets(&bot)?.join("licenses/NOTICES.txt"))
        .await
        .map_err(|_| missing())?;
    const LIMIT: usize = 4 * 1024 * 1024;
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| missing())?;
    if bytes.is_empty() || bytes.len() > LIMIT || std::str::from_utf8(&bytes).is_err() {
        return Err(missing());
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response())
}

/// The type of an app file, by extension; others are not served.
fn asset_type(file: &str) -> Option<&'static str> {
    let (_, extension) = file.rsplit_once('.')?;
    Some(match extension {
        "js" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => return None,
    })
}

/// The app's built files. Their names are content hashes, so they may be kept.
async fn asset(
    State(bot): State<Arc<Bot>>,
    Path((token, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let missing = || ApiError(StatusCode::NOT_FOUND, "no such file".to_owned());
    // Plain names only: nothing can reach outside the assets folder.
    let plain = !file.starts_with('.')
        && file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    let kind = asset_type(&file).filter(|_| plain).ok_or_else(missing)?;
    let directory = assets(&bot).map_err(|_| missing())?;
    let bytes = tokio::fs::read(directory.join("assets").join(&file))
        .await
        .map_err(|_| missing())?;
    Ok((
        [
            (header::CONTENT_TYPE, kind),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
struct ArtQuery {
    #[serde(default = "default_art_size")]
    size: u32,
    /// The library's `library_keys` entry from the state: proves the URL
    /// names the current library, so the browser may keep the answer.
    v: Option<String>,
}

fn default_art_size() -> u32 {
    300
}

/// The edge lengths covers are made in. A request gets the largest step that
/// fits, so the page's few sizes share cache entries and worker jobs, and its
/// check that pixels are no larger than asked still holds.
const COVER_SIZES: [u32; 7] = [32, 64, 96, 128, 200, 300, 400];

fn snap_cover_size(requested: u32) -> u32 {
    let wanted = requested.min(nuschl_ipc::MAX_THUMBNAIL_EDGE);
    COVER_SIZES
        .into_iter()
        .rev()
        .find(|size| *size <= wanted && *size <= nuschl_ipc::MAX_THUMBNAIL_EDGE)
        .unwrap_or(COVER_SIZES[0])
}

fn content_worker_path(executable: &std::path::Path) -> Result<PathBuf, ApiError> {
    executable
        .parent()
        .and_then(|bin| bin.parent())
        .map(|root| root.join("libexec/nuschl-musicbot/nuschl-content-worker"))
        .ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the artwork worker is unavailable".to_owned(),
            )
        })
}

/// Only PNGs the confined content worker made from the decoded pixels leave
/// this route, checked again here: encoded library art never reaches the
/// browser. The owned task retains concurrency through worker
/// completion/reaping even after its HTTP consumer disappears. Answers come
/// from the shared cache when they can; only misses take a job.
async fn art(
    State(bot): State<Arc<Bot>>,
    Path((token, library, id)): Path<(String, usize, String)>,
    Query(query): Query<ArtQuery>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let Some(source) = bot.libraries.get(library) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "no such library".to_owned(),
        ));
    };
    // A URL for another library (the configuration changed) must not be
    // kept by the browser under this library's covers.
    let versioned = match query.v.as_deref() {
        None => false,
        Some(key) if key == source.key() => true,
        Some(_) => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "the library changed; reload the page".to_owned(),
            ));
        }
    };
    let size = snap_cover_size(query.size);
    let key: CoverKey = (source.key().to_owned(), id, size);
    let cached = bot.covers.get(&key).await;
    let body = match cached {
        Some(body) => body,
        None => {
            let (libraries, id) = (bot.libraries.clone(), key.1.clone());
            let image = move |fetch| library_image(libraries, library, id, size, fetch);
            fetch_cover(&bot, key, image, worker_thumbnail).await?
        }
    };
    owner(&bot, &token)?;
    let caching = if versioned {
        "private, max-age=86400"
    } else {
        "private, no-store"
    };
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, caching),
        ],
        body,
    )
        .into_response())
}

/// A library's encoded cover; the call holds `fetch` until its transport is
/// done. A busy library gives 503; other failures give 404.
async fn library_image(
    libraries: Arc<Vec<Library>>,
    library: usize,
    id: String,
    size: u32,
    fetch: SemaphorePermit<'static>,
) -> Result<Vec<u8>, ApiError> {
    blocking_call(LIBRARY_TIMEOUT, move || {
        let _fetch = fetch;
        libraries[library].cover_art(&id, size)
    })
    .await
    .map(|image| image.bytes)
    .map_err(|failure| match library_failure(failure) {
        error if error.0 == StatusCode::SERVICE_UNAVAILABLE => error,
        error => ApiError(StatusCode::NOT_FOUND, error.1),
    })
}

/// Makes an uncached cover and caches it, even when the HTTP client
/// has gone by then. With at most [`MAX_COVER_WAITERS`] requests on the way, a
/// request waits up to [`COVER_WAIT`] for one of [`MAX_COVER_FETCHES`] fetch
/// permits; a cover another request finished meanwhile comes from the cache,
/// and otherwise `image` fetches the encoded cover, holding the permit until
/// its transport is done. A PNG the worker already made from the same bytes
/// at this size comes from memory. Otherwise the request waits for the image's
/// turn, so identical images arriving together take one job, and then up to
/// [`COVER_WAIT`] for one of [`MAX_COVER_JOBS`] jobs, in which `convert` makes
/// the PNG.
async fn fetch_cover<I, F, C, R>(
    bot: &Arc<Bot>,
    key: CoverKey,
    image: I,
    convert: C,
) -> Result<Bytes, ApiError>
where
    I: FnOnce(SemaphorePermit<'static>) -> F,
    F: Future<Output = Result<Vec<u8>, ApiError>>,
    C: FnOnce(Vec<u8>, u32) -> R + Send + 'static,
    R: Future<Output = Result<Bytes, ApiError>> + Send + 'static,
{
    let busy = || {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the artwork worker is busy".to_owned(),
        )
    };
    let waiting = COVER_WAITERS.try_acquire().map_err(|_| busy())?;
    let fetch = tokio::time::timeout(COVER_WAIT, COVER_FETCHES.acquire())
        .await
        .map_err(|_| busy())?
        .map_err(|_| busy())?;
    if let Some(body) = bot.covers.get(&key).await {
        return Ok(body);
    }
    let image = image(fetch).await?;
    let made: ImageKey = (Sha256::digest(&image).into(), key.2);
    if let Some(body) = bot.covers.made(&made) {
        return Ok(keep(bot, key, body).await);
    }
    let turn = tokio::time::timeout(COVER_WAIT, image_turn(made))
        .await
        .map_err(|_| busy())?;
    if let Some(body) = bot.covers.made(&made) {
        return Ok(keep(bot, key, body).await);
    }
    let job = tokio::time::timeout(COVER_WAIT, COVER_JOBS.acquire())
        .await
        .map_err(|_| busy())?
        .map_err(|_| busy())?;
    drop(waiting);
    let owned = bot.clone();
    let task = tokio::spawn(async move {
        let (_turn, _job) = (turn, job);
        let body = convert(image, made.1).await?;
        owned.covers.remember(made, body.clone());
        owned.covers.insert(&key, &body).await;
        Ok::<_, ApiError>(body)
    });
    match tokio::time::timeout(COVER_JOB_TIMEOUT, task).await {
        Ok(Ok(result)) => result,
        _ => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "the artwork worker did not finish in time".to_owned(),
        )),
    }
}

/// The right to make the PNG for `image`; its holder is the only request
/// making it. Entries nobody holds or waits for leave the map here.
async fn image_turn(image: ImageKey) -> OwnedMutexGuard<()> {
    let lock = {
        let mut making = MAKING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        making.retain(|_, lock| Arc::strong_count(lock) > 1);
        making.entry(image).or_default().clone()
    };
    lock.lock_owned().await
}

/// Caches a PNG made for another key under `key` as well, even when the HTTP
/// client has gone by then.
async fn keep(bot: &Arc<Bot>, key: CoverKey, body: Bytes) -> Bytes {
    let (owned, kept) = (bot.clone(), body.clone());
    let _ = tokio::spawn(async move { owned.covers.insert(&key, &kept).await }).await;
    body
}

/// Has the confined content worker re-encode `image` as a PNG of at most
/// `size` pixels per edge.
async fn worker_thumbnail(image: Vec<u8>, size: u32) -> Result<Bytes, ApiError> {
    let unavailable = || {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the artwork worker is unavailable".to_owned(),
        )
    };
    let worker = content_worker_path(&std::env::current_exe().map_err(|_| unavailable())?)?;
    if !worker.is_file() {
        return Err(unavailable());
    }
    nuschl_ipc::host::thumbnail(&worker, &image, size, size)
        .await
        .map(|thumbnail| Bytes::from(thumbnail.png))
        .map_err(|_| {
            ApiError(
                StatusCode::NOT_FOUND,
                "the cover image is invalid or unavailable".to_owned(),
            )
        })
}

#[derive(Serialize)]
struct StateView {
    you: String,
    libraries: Vec<String>,
    /// Per library, parallel to `libraries`: whether it is known to offer
    /// sonic similarity. Read from what is already known, never asked.
    sonic: Vec<bool>,
    /// Per library: identifies it in shared links and cover URLs (`v`).
    library_keys: Vec<String>,
    players: Vec<PlayerView>,
    channels: Vec<ChannelView>,
    rating_revision: u64,
    playlists: PlaylistStateView,
    /// Adding music, when configured: the library added albums arrive in and
    /// a revision that advances when the import list changes.
    import: Option<ImportStateView>,
}

/// Personal playlists: a revision that advances after every change, and
/// whether this viewer may own and edit playlists (their Mumble client has a
/// certificate).
#[derive(Serialize)]
struct PlaylistStateView {
    revision: u64,
    can_own: bool,
}

#[derive(Serialize)]
struct ImportStateView {
    library: usize,
    revision: u64,
}

#[derive(Serialize)]
struct PlayerView {
    name: String,
    paused: bool,
    volume: u8,
    ducking: DuckingView,
    ducked: bool,
    listeners: usize,
    error: Option<String>,
    current: Option<CurrentView>,
    queue: Vec<EntryView>,
    history_revision: u64,
    /// DJ mode, while it is on.
    dj: Option<DjView>,
    /// DJ mode's queue per deck, kept while DJ mode is off.
    deck_queues: [Vec<EntryView>; DECKS],
}

#[derive(Serialize)]
struct DjView {
    decks: Vec<DeckView>,
    /// −1 (deck A) to 1 (deck B).
    crossfader: f32,
    curve: Curve,
    transition: Option<TransitionView>,
    auto_load: bool,
    notice: Option<String>,
}

#[derive(Serialize)]
struct DeckView {
    entry: Option<EntryView>,
    /// Song time; it advances `tempo` percent faster than real time.
    position_ms: u64,
    playing: bool,
    buffering: bool,
    advancing: bool,
    cue_ms: u64,
    volume: u8,
    eq: EqView,
    filter: f32,
    /// Percent.
    tempo: f64,
    tempo_range: u8,
    /// The song's BPM before the tempo: the library's, else the estimate.
    bpm: Option<f64>,
    /// "library" or "estimate".
    bpm_source: Option<&'static str>,
    /// The BPM estimate or beat grid is being computed.
    analysing: bool,
    /// How much of the song is decoded in memory; null while the deck streams.
    loaded_ms: Option<u64>,
    /// The whole song is in memory.
    preloaded: bool,
    #[serde(rename = "loop")]
    looping: Option<LoopView>,
    quantize: bool,
    grid: Option<GridView>,
    error: Option<String>,
    cue_listeners: usize,
}

#[derive(Serialize)]
struct LoopView {
    start_ms: f64,
    end_ms: Option<f64>,
    active: bool,
}

#[derive(Serialize)]
struct GridView {
    bpm: f64,
    first_beat_ms: f64,
}

#[derive(Serialize)]
struct EqView {
    low: f32,
    mid: f32,
    high: f32,
}

#[derive(Serialize)]
struct TransitionView {
    to: usize,
    duration_ms: u64,
    elapsed_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct DuckingView {
    enabled: bool,
    level: u8,
    hold_ms: u64,
    sensitivity: u8,
}

#[derive(Serialize)]
struct CurrentView {
    entry: EntryView,
    position_ms: u64,
    advancing: bool,
    buffering: bool,
}

#[derive(Serialize)]
struct EntryView {
    id: u64,
    added_by: String,
    track: Track,
}

#[derive(Serialize)]
struct ChannelView {
    name: String,
    server: String,
    connected: bool,
    channel: Option<String>,
    listeners: usize,
    error: Option<String>,
    player: usize,
}

/// Everything the app shows, as one snapshot.
fn snapshot(bot: &Bot, you: &Owner) -> StateView {
    let players = bot
        .players
        .iter()
        .map(|player| {
            let listeners = player.listeners();
            let state = player.state();
            let entry = |entry: &crate::player::Entry| EntryView {
                id: entry.id,
                added_by: entry.added_by.clone(),
                track: entry.track.clone(),
            };
            PlayerView {
                name: player.name.clone(),
                paused: state.paused,
                volume: state.volume,
                ducking: DuckingView {
                    enabled: state.ducking.enabled,
                    level: state.ducking.level,
                    hold_ms: state.ducking.hold.as_millis() as u64,
                    sensitivity: state.ducking.sensitivity,
                },
                ducked: player.ducked(&state.ducking),
                listeners,
                error: state.error.clone(),
                current: state.current.as_ref().map(|playing| CurrentView {
                    entry: entry(&playing.entry),
                    position_ms: playing.elapsed().as_millis() as u64,
                    advancing: !state.paused && listeners > 0 && !playing.buffering,
                    buffering: playing.buffering,
                }),
                queue: state.queue.iter().map(entry).collect(),
                history_revision: state.next_history_id.saturating_sub(1),
                deck_queues: state
                    .deck_queues
                    .each_ref()
                    .map(|queue| queue.iter().map(entry).collect()),
                dj: state.dj.as_ref().map(|dj| {
                    let cue_listeners = player.cue_listeners();
                    let running =
                        !state.paused && (listeners > 0 || cue_listeners.iter().any(|n| *n > 0));
                    DjView {
                        decks: dj
                            .decks
                            .iter()
                            .zip(cue_listeners)
                            .map(|(deck, cue_listeners)| {
                                let buffering =
                                    deck.track.as_ref().is_some_and(|playing| playing.buffering);
                                DeckView {
                                    entry: deck.track.as_ref().map(|playing| entry(&playing.entry)),
                                    position_ms: deck.position_ms(),
                                    playing: deck.playing,
                                    buffering,
                                    advancing: running
                                        && deck.playing
                                        && deck.track.is_some()
                                        && !buffering,
                                    cue_ms: deck.cue_ms(),
                                    volume: deck.volume,
                                    eq: EqView {
                                        low: deck.eq[0],
                                        mid: deck.eq[1],
                                        high: deck.eq[2],
                                    },
                                    filter: deck.filter,
                                    tempo: (f64::from(deck.tempo) * 1e6).round() / 1e4,
                                    tempo_range: deck.range,
                                    bpm: deck.base_bpm().map(|bpm| (bpm * 10.0).round() / 10.0),
                                    bpm_source: deck.bpm_from_library().map(|library| {
                                        if library { "library" } else { "estimate" }
                                    }),
                                    analysing: deck.analysing,
                                    loaded_ms: deck.loaded.map(|frames| frames / 48),
                                    preloaded: deck.preloaded,
                                    looping: deck.looping.map(|looping| LoopView {
                                        start_ms: looping.start / 48.0,
                                        end_ms: looping.end.map(|end| end / 48.0),
                                        active: looping.active,
                                    }),
                                    quantize: deck.quantize,
                                    grid: deck.grid.map(|grid| GridView {
                                        bpm: grid.bpm,
                                        first_beat_ms: grid.first / 48.0,
                                    }),
                                    error: deck.error.clone(),
                                    cue_listeners,
                                }
                            })
                            .collect(),
                        crossfader: dj.crossfader,
                        curve: dj.curve,
                        transition: dj.transition.map(|transition| TransitionView {
                            to: transition.to,
                            duration_ms: transition.duration_ms(),
                            elapsed_ms: transition.elapsed_ms(),
                        }),
                        auto_load: dj.auto_load,
                        notice: dj.notice.clone(),
                    }
                }),
            }
        })
        .collect();
    let channels = bot
        .connections
        .iter()
        .map(|connection| {
            let status = connection.status().clone();
            ChannelView {
                name: connection.name.clone(),
                server: connection.server.clone(),
                connected: status.connected,
                channel: status.channel,
                listeners: status.listeners,
                error: status.error,
                player: *connection.player.borrow(),
            }
        })
        .collect();
    StateView {
        you: you.name.clone(),
        libraries: bot
            .libraries
            .iter()
            .map(|library| library.name.clone())
            .collect(),
        sonic: bot
            .libraries
            .iter()
            .map(|library| library.supports_sonic_similarity_cached())
            .collect(),
        library_keys: bot
            .libraries
            .iter()
            .map(|library| library.key().to_owned())
            .collect(),
        players,
        channels,
        rating_revision: bot.rating_revision.load(Ordering::SeqCst),
        playlists: PlaylistStateView {
            revision: bot.playlists.revision(),
            can_own: crate::playlists::can_own(you),
        },
        import: bot.adding.as_ref().map(|adding| ImportStateView {
            library: adding.library,
            revision: adding.revision.load(Ordering::SeqCst),
        }),
    }
}

async fn state(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Json<StateView>, ApiError> {
    let owner = owner(&bot, &token)?;
    Ok(Json(snapshot(&bot, &owner)))
}

#[derive(Deserialize)]
struct HistoryQuery {
    player: usize,
    before: Option<u64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct HistoryPage {
    items: Vec<HistoryView>,
    next_before: Option<u64>,
    oldest_id: Option<u64>,
    revision: u64,
}

#[derive(Serialize)]
struct HistoryView {
    id: u64,
    entry: HistoryEntryView,
    library_name: Option<String>,
    started_at_ms: Option<i64>,
    ended_at_ms: i64,
    played_ms: u64,
    position_ms: u64,
    outcome: crate::player::HistoryOutcome,
    error: Option<String>,
}

#[derive(Serialize)]
struct HistoryEntryView {
    id: u64,
    added_by: String,
    track: HistoryTrackView,
}

#[derive(Serialize)]
struct HistoryTrackView {
    library: Option<usize>,
    id: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    album_id: Option<String>,
    artist_id: Option<String>,
    duration: Option<u32>,
    track: Option<u32>,
    suffix: Option<String>,
    content_type: Option<String>,
    cover_art: Option<String>,
}

fn history_view(
    item: &crate::player::HistoryItem,
    libraries: &[crate::config::Library],
) -> HistoryView {
    let track = &item.entry.track;
    let library = item.source.as_ref().and_then(|source| {
        libraries.iter().position(|library| {
            library.name == source.name && library.url.trim_end_matches('/') == source.url
        })
    });
    HistoryView {
        id: item.id,
        entry: HistoryEntryView {
            id: item.entry.id,
            added_by: item.entry.added_by.clone(),
            track: HistoryTrackView {
                library,
                id: track.id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                album_id: track.album_id.clone(),
                artist_id: track.artist_id.clone(),
                duration: track.duration,
                track: track.track,
                suffix: track.suffix.clone(),
                content_type: track.content_type.clone(),
                cover_art: track.cover_art.clone(),
            },
        },
        library_name: item.source.as_ref().map(|source| source.name.clone()),
        started_at_ms: item.started_at_ms,
        ended_at_ms: item.ended_at_ms,
        played_ms: item.played_ms,
        position_ms: item.position_ms,
        outcome: item.outcome,
        error: item.error.clone(),
    }
}

async fn history(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryPage>, ApiError> {
    owner(&bot, &token)?;
    let limit = query.limit.unwrap_or(50).min(100);
    if limit == 0 {
        return Err(bad("history limit must be greater than zero"));
    }
    let player = bot
        .players
        .get(query.player)
        .ok_or_else(|| bad("no such queue"))?;
    let state = player.state();
    let mut older = state
        .history
        .iter()
        .filter(|item| query.before.is_none_or(|before| item.id < before));
    let items: Vec<_> = older
        .by_ref()
        .take(limit)
        .map(|item| history_view(item, &bot.config.libraries))
        .collect();
    let next_before = if older.next().is_some() {
        items.last().map(|item| item.id)
    } else {
        None
    };
    Ok(Json(HistoryPage {
        items,
        next_before,
        oldest_id: state.history.back().map(|item| item.id),
        revision: state.next_history_id.saturating_sub(1),
    }))
}

#[derive(Deserialize)]
struct ListenQuery {
    player: usize,
}

/// An admitted body owns its producer; disconnecting cancels all listening work.
struct ListenBody {
    records: mpsc::Receiver<Bytes>,
    producer: tokio::task::JoinHandle<()>,
}

impl Stream for ListenBody {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.producer.is_finished() {
            self.records.close();
            return Poll::Ready(None);
        }
        self.records.poll_recv(cx).map(|record| record.map(Ok))
    }
}

impl Drop for ListenBody {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

/// Keeping the subscription and permit together prevents early capacity release
/// when a body is dropped but cancellation has not run yet.
struct ListenLease {
    subscription: BrowserSubscription,
    _permit: SemaphorePermit<'static>,
}

async fn listen(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ListenQuery>,
) -> Result<Response, ApiError> {
    listen_response(bot, token, query.player, Listen::Room, &LISTEN_STREAMS)
}

#[derive(Deserialize)]
struct CueQuery {
    player: usize,
    deck: usize,
}

/// A DJ deck's audio before its channel fader, for headphones in a browser.
async fn cue(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<CueQuery>,
) -> Result<Response, ApiError> {
    listen_response(
        bot,
        token,
        query.player,
        Listen::Cue(query.deck),
        &CUE_STREAMS,
    )
}

fn listen_response(
    bot: Arc<Bot>,
    token: String,
    index: usize,
    stream: Listen,
    admission: &'static Semaphore,
) -> Result<Response, ApiError> {
    let token = Uuid::parse_str(&token)
        .ok()
        .filter(|token| bot.tokens.is_valid(token))
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, INVALID_LINK.to_owned()))?;
    let player = bot.players.get(index).ok_or_else(|| bad("no such queue"))?;
    if let Listen::Cue(deck) = stream
        && deck >= DECKS
    {
        return Err(bad("no such deck"));
    }
    let permit = admission.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many browser listeners".to_owned(),
        )
    })?;
    let lease = ListenLease {
        subscription: player.subscribe_browser(stream),
        _permit: permit,
    };
    // Only one record may leave the shared live fanout before the body polls.
    // A separate task checks revocation even while HTTP backpressure stops polls.
    let (send, records) = mpsc::channel(1);
    let producer = tokio::spawn(async move {
        let mut lease = lease;
        let mut check = tokio::time::interval(Duration::from_secs(1));
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_record = tokio::time::Instant::now();
        loop {
            if !bot.tokens.is_valid(&token) {
                break;
            }
            let space = tokio::select! {
                biased;
                _ = check.tick() => continue,
                result = send.reserve() => match result {
                    Ok(space) => space,
                    Err(_) => break,
                },
            };
            let record = tokio::select! {
                biased;
                _ = check.tick() => continue,
                _ = send.closed() => break,
                record = lease.subscription.recv() => match record {
                    Some(record) => record,
                    None => break,
                },
                _ = tokio::time::sleep_until(last_record + LISTEN_KEEPALIVE) => {
                    Bytes::from_static(KEEPALIVE_RECORD)
                }
            };
            if !bot.tokens.is_valid(&token) {
                break;
            }
            space.send(record);
            last_record = tokio::time::Instant::now();
        }
    });
    let body = Body::from_stream(ListenBody { records, producer });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
            (HeaderName::from_static("x-accel-buffering"), "no"),
            (HeaderName::from_static("x-audio-format"), "f32le"),
            (HeaderName::from_static("x-audio-rate"), "48000"),
            (HeaderName::from_static("x-audio-channels"), "2"),
            (HeaderName::from_static("x-audio-frame-samples"), "960"),
        ],
        body,
    )
        .into_response())
}

/// The state now and after every change, until the link is revoked.
async fn events(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let permit = SSE_WATCHERS.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many live music viewers".to_owned(),
        )
    })?;
    let changes = bot.changes.subscribe();
    let stream = state_stream(bot, token, changes, permit);
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    // Proxies such as nginx must pass events on at once.
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

fn state_stream(
    bot: Arc<Bot>,
    token: String,
    changes: tokio::sync::watch::Receiver<u64>,
    permit: tokio::sync::SemaphorePermit<'static>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold(
        (bot, token, changes, None::<tokio::time::Instant>, permit),
        |(bot, token, mut changes, last, permit)| async move {
            if let Some(last) = last {
                tokio::select! {
                    result = changes.changed() => { result.ok()?; }
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                tokio::time::sleep(COALESCE).await;
                tokio::time::sleep_until(last + MIN_EVENT_GAP).await;
            }
            changes.borrow_and_update();
            let owner = owner(&bot, &token).ok()?;
            let event = Event::default()
                .event("state")
                .json_data(snapshot(&bot, &owner))
                .ok()?;
            let sent = Some(tokio::time::Instant::now());
            Some((Ok(event), (bot, token, changes, sent, permit)))
        },
    )
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Song,
    Album,
    Playlist,
    /// The songs that sound like `id`.
    Similar,
    /// The songs leading from `id` to `to` by sound.
    Path,
    /// One page of a genre's songs: `id` is the genre's name, `page` the page.
    Genre,
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Command {
    Pause {
        player: usize,
    },
    Resume {
        player: usize,
    },
    Skip {
        player: usize,
    },
    Clear {
        player: usize,
    },
    Seek {
        player: usize,
        position_ms: u64,
    },
    Volume {
        player: usize,
        volume: u8,
    },
    Ducking {
        player: usize,
        #[serde(flatten)]
        ducking: DuckingView,
    },
    Remove {
        player: usize,
        entry: u64,
    },
    Move {
        player: usize,
        entry: u64,
        index: usize,
    },
    Add {
        player: usize,
        kind: Kind,
        library: usize,
        id: String,
        /// The song a `path` ends with.
        to: Option<String>,
        /// The page of a `genre`'s songs, from 0.
        page: Option<u32>,
        index: Option<usize>,
    },
    Attach {
        channel: usize,
        player: usize,
    },
}

impl Command {
    fn player(&self) -> usize {
        match self {
            Self::Pause { player }
            | Self::Resume { player }
            | Self::Skip { player }
            | Self::Clear { player }
            | Self::Seek { player, .. }
            | Self::Volume { player, .. }
            | Self::Ducking { player, .. }
            | Self::Remove { player, .. }
            | Self::Move { player, .. }
            | Self::Add { player, .. }
            | Self::Attach { player, .. } => *player,
        }
    }
}

/// A change from the app; the new state reaches every viewer through `events`.
async fn command(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    // JSON only: a form on another site cannot send it without the browser asking first.
    Json(command): Json<Command>,
) -> Result<StatusCode, ApiError> {
    let owner = owner(&bot, &token)?;
    let player = bot
        .players
        .get(command.player())
        .ok_or_else(|| bad("no such queue"))?;
    match command {
        Command::Pause { .. } => player.set_paused(true),
        Command::Resume { .. } => player.set_paused(false),
        Command::Skip { .. } => player.skip(),
        Command::Clear { .. } => player.clear(),
        Command::Seek { position_ms, .. } => player.seek(Duration::from_millis(position_ms)),
        Command::Volume { volume, .. } => player.set_volume(volume),
        Command::Ducking { ducking, .. } => player.set_ducking(Ducking {
            enabled: ducking.enabled,
            level: ducking.level,
            hold: Duration::from_millis(ducking.hold_ms).min(MAX_DUCK_HOLD),
            sensitivity: ducking.sensitivity,
        }),
        Command::Remove { entry, .. } => player.remove(entry),
        Command::Move { entry, index, .. } => player.move_to(entry, index),
        Command::Add {
            kind,
            library,
            id,
            to,
            page,
            index,
            ..
        } => {
            let to = match kind {
                Kind::Path => to.ok_or_else(|| bad("a sonic path needs the song it ends with"))?,
                _ => String::new(),
            };
            let id = match kind {
                Kind::Genre => genre_name(id)?,
                _ => id,
            };
            let offset = page.unwrap_or(0).min(MAX_PAGE) * GENRE_SONGS_PAGE;
            let sonic = |matches: Vec<SonicMatch>| -> Vec<Track> {
                matches.into_iter().map(|found| found.track).collect()
            };
            let tracks = ask(&bot, library, move |library| match kind {
                Kind::Album => library.album(&id).map(|(_, songs)| songs),
                Kind::Playlist => library.playlist(&id).map(|(_, songs)| songs),
                Kind::Song => library.song(&id).map(|song| vec![song]),
                Kind::Similar => {
                    let matches = library.sonic_similar(&id)?;
                    Ok(sonic(distinct_similar(matches, &library.song(&id)?)))
                }
                Kind::Path => library.sonic_path(&id, &to).map(sonic),
                Kind::Genre => library.genre_songs(&id, GENRE_SONGS_PAGE, offset),
            })
            .await?;
            let tracks = match kind {
                Kind::Path => {
                    let identity = bot.libraries[library].identity();
                    let state = player.state();
                    let playing = state
                        .now_playing()
                        .map(|playing| (playing.source.as_ref(), playing.entry.track.id.as_str()));
                    continue_path(tracks, playing, &identity)
                }
                _ => tracks,
            };
            let added = player.enqueue(tracks, &owner.name, index);
            tracing::info!("{} added {added} song(s) to {}", owner.name, player.name);
        }
        Command::Attach { channel, player } => {
            let connection = bot
                .connections
                .get(channel)
                .ok_or_else(|| bad("no such channel"))?;
            connection.player.send_replace(player);
            bot.changes.notify();
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Columns one spectrum request returns at most (20 s).
const MAX_SPECTRUM_COLUMNS: usize = 2_000;
/// Columns of a song's overview.
const OVERVIEW_COLUMNS: usize = 1_000;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SpectrumView {
    Coarse,
    Fine,
}

#[derive(Deserialize)]
struct SpectrumQuery {
    player: usize,
    deck: usize,
    entry: u64,
    view: Option<SpectrumView>,
    from: Option<usize>,
    count: Option<usize>,
}

/// The in-memory song of `deck` when it still is `entry`, and the song's
/// expected column count (0 when its length is unknown).
fn deck_buffer(
    bot: &Bot,
    token: &str,
    query: &SpectrumQuery,
) -> Result<(Arc<crate::preload::Buffer>, usize), ApiError> {
    owner(bot, token)?;
    let player = bot
        .players
        .get(query.player)
        .ok_or_else(|| bad("no such queue"))?;
    deck_index(query.deck)?;
    let total = {
        let state = player.state();
        let track = state
            .dj
            .as_ref()
            .and_then(|dj| dj.decks[query.deck].track.as_ref())
            .filter(|playing| playing.entry.id == query.entry)
            .ok_or_else(|| {
                ApiError(
                    StatusCode::CONFLICT,
                    "That deck plays another song now.".to_owned(),
                )
            })?;
        track.entry.track.duration.map_or(0, |seconds| {
            seconds as usize * crate::spectrum::COLUMNS_PER_SECOND
        })
    };
    let buffer = bot
        .preload
        .as_ref()
        .and_then(|pool| pool.buffer(query.player, query.entry))
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "This song plays from the library; its waveform needs it in memory.".to_owned(),
            )
        })?;
    Ok((buffer, total))
}

fn spectrum_response(
    bytes: Vec<u8>,
    from: usize,
    columns: usize,
    total: usize,
    bands: usize,
) -> Response {
    let number = |value: usize| HeaderValue::from(value as u64);
    let mut response = (
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(HeaderName::from_static("x-spectrum-from"), number(from));
    headers.insert(
        HeaderName::from_static("x-spectrum-columns"),
        number(columns),
    );
    headers.insert(HeaderName::from_static("x-spectrum-total"), number(total));
    headers.insert(HeaderName::from_static("x-spectrum-bands"), number(bands));
    response
}

/// Band energies of a deck's song, at most [`MAX_SPECTRUM_COLUMNS`] columns
/// from `from`: one byte per band and column (see docs/musicbot.md).
async fn spectrum(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<SpectrumQuery>,
) -> Result<Response, ApiError> {
    let count = query.count.unwrap_or(MAX_SPECTRUM_COLUMNS);
    if count == 0 || count > MAX_SPECTRUM_COLUMNS {
        return Err(bad(format!(
            "count must be from 1 to {MAX_SPECTRUM_COLUMNS}"
        )));
    }
    let (buffer, total) = deck_buffer(&bot, &token, &query)?;
    let from = query.from.unwrap_or(0);
    let spectrum = buffer.spectrum();
    let to = from.saturating_add(count);
    let (bytes, bands) = match query.view.unwrap_or(SpectrumView::Coarse) {
        SpectrumView::Coarse => (
            spectrum.coarse(from, to).to_vec(),
            crate::spectrum::COARSE_BANDS,
        ),
        SpectrumView::Fine => (
            spectrum.fine(from, to).to_vec(),
            crate::spectrum::FINE_BANDS,
        ),
    };
    Ok(spectrum_response(
        bytes,
        from,
        spectrum.columns(),
        total,
        bands,
    ))
}

/// The whole song of a deck in [`OVERVIEW_COLUMNS`] coarse columns.
async fn spectrum_overview(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<SpectrumQuery>,
) -> Result<Response, ApiError> {
    let (buffer, total) = deck_buffer(&bot, &token, &query)?;
    let spectrum = buffer.spectrum();
    let columns = spectrum.columns();
    let bytes = spectrum.overview(OVERVIEW_COLUMNS, total.max(columns));
    Ok(spectrum_response(
        bytes,
        0,
        columns,
        total,
        crate::spectrum::COARSE_BANDS,
    ))
}

/// The longest seek a deck accepts, as the decoder's skip limit (three hours).
const MAX_DECK_SEEK_MS: u64 = 3 * 60 * 60 * 1000;
/// Song IDs a deck accepts from a library, in bytes.
const MAX_DECK_SONG_ID: usize = 128;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum LoopStep {
    Loop,
    Beat,
}

/// The beat loop lengths a deck offers.
const BEAT_LOOPS: [f64; 8] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Band {
    Low,
    Mid,
    High,
}

/// A DJ control; continuous ones arrive up to thirty times a second.
#[derive(Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
enum DjControl {
    Mode {
        on: bool,
    },
    /// A library song, album or playlist (`library`, `id`, `kind`) or an
    /// entry of the main queue or a deck queue (`entry`).
    Load {
        deck: usize,
        library: Option<usize>,
        id: Option<String>,
        kind: Option<Kind>,
        entry: Option<u64>,
    },
    /// Adds a library song, album or playlist to a deck's queue.
    QueueAdd {
        deck: usize,
        library: usize,
        id: String,
        kind: Option<Kind>,
        index: Option<usize>,
    },
    /// Moves an entry into a deck's queue (`deck`) or the main queue (null).
    QueueMove {
        entry: u64,
        deck: Option<usize>,
        index: usize,
    },
    QueueRemove {
        entry: u64,
    },
    QueueClear {
        deck: usize,
    },
    LoopIn {
        deck: usize,
    },
    LoopOut {
        deck: usize,
    },
    LoopToggle {
        deck: usize,
    },
    BeatLoop {
        deck: usize,
        beats: f64,
    },
    LoopResize {
        deck: usize,
        factor: f64,
    },
    LoopMove {
        deck: usize,
        by: LoopStep,
        direction: i8,
    },
    LoopSet {
        deck: usize,
        start_ms: f64,
        end_ms: f64,
    },
    Quantize {
        deck: usize,
        on: bool,
    },
    Eject {
        deck: usize,
    },
    Play {
        deck: usize,
        playing: bool,
    },
    Seek {
        deck: usize,
        position_ms: u64,
    },
    SetCue {
        deck: usize,
    },
    JumpToCue {
        deck: usize,
    },
    Volume {
        deck: usize,
        volume: u8,
    },
    Eq {
        deck: usize,
        band: Band,
        db: f32,
    },
    Filter {
        deck: usize,
        value: f32,
    },
    /// Percent.
    Tempo {
        deck: usize,
        tempo: f32,
    },
    TempoRange {
        deck: usize,
        range: u8,
    },
    Sync {
        deck: usize,
    },
    Crossfader {
        position: f32,
    },
    Curve {
        curve: Curve,
    },
    Transition {
        seconds: u32,
    },
    AutoLoad {
        on: bool,
    },
}

#[derive(Deserialize)]
struct DjCommand {
    player: usize,
    #[serde(flatten)]
    control: DjControl,
}

impl DjControl {
    /// Range checks before the control reaches the player.
    fn validate(&self) -> Result<(), ApiError> {
        let within = |value: f32, low: f32, high: f32, what: &str| {
            if value.is_finite() && (low..=high).contains(&value) {
                Ok(())
            } else {
                Err(bad(format!("{what} must be from {low} to {high}")))
            }
        };
        match self {
            Self::Load {
                deck,
                library,
                id,
                kind,
                entry,
            } => {
                deck_index(*deck)?;
                match (library, id, entry) {
                    (Some(_), Some(id), None) => deck_item(id, *kind)?,
                    (None, None, Some(_)) if kind.is_none() => {}
                    _ => return Err(bad("Name a library song or a queue entry.")),
                }
            }
            Self::QueueAdd { deck, id, kind, .. } => {
                deck_index(*deck)?;
                deck_item(id, *kind)?;
            }
            Self::QueueMove { deck, .. } => {
                if let Some(deck) = deck {
                    deck_index(*deck)?;
                }
            }
            Self::QueueClear { deck }
            | Self::LoopIn { deck }
            | Self::LoopOut { deck }
            | Self::LoopToggle { deck }
            | Self::Quantize { deck, .. } => deck_index(*deck)?,
            Self::BeatLoop { deck, beats } => {
                deck_index(*deck)?;
                if !BEAT_LOOPS.contains(beats) {
                    return Err(bad("a beat loop is 1/4, 1/2, 1, 2, 4, 8, 16 or 32 beats"));
                }
            }
            Self::LoopResize { deck, factor } => {
                deck_index(*deck)?;
                if *factor != 0.5 && *factor != 2.0 {
                    return Err(bad("a loop halves (0.5) or doubles (2)"));
                }
            }
            Self::LoopMove {
                deck, direction, ..
            } => {
                deck_index(*deck)?;
                if direction.abs() != 1 {
                    return Err(bad("a loop moves back (-1) or forward (1)"));
                }
            }
            Self::LoopSet {
                deck,
                start_ms,
                end_ms,
            } => {
                deck_index(*deck)?;
                for value in [start_ms, end_ms] {
                    if !value.is_finite() || !(0.0..=MAX_DECK_SEEK_MS as f64).contains(value) {
                        return Err(bad("loop points lie within the first three hours"));
                    }
                }
            }
            Self::QueueRemove { .. } => {}
            Self::Eject { deck }
            | Self::Play { deck, .. }
            | Self::SetCue { deck }
            | Self::JumpToCue { deck }
            | Self::Sync { deck } => deck_index(*deck)?,
            Self::Seek { deck, position_ms } => {
                deck_index(*deck)?;
                if *position_ms > MAX_DECK_SEEK_MS {
                    return Err(bad("a deck seeks at most three hours into a song"));
                }
            }
            Self::Volume { deck, volume } => {
                deck_index(*deck)?;
                if *volume > 100 {
                    return Err(bad("volume must be from 0 to 100"));
                }
            }
            Self::Eq { deck, db, .. } => {
                deck_index(*deck)?;
                within(*db, playback::EQ_MIN_DB, playback::EQ_MAX_DB, "EQ")?;
            }
            Self::Filter { deck, value } => {
                deck_index(*deck)?;
                within(*value, -1.0, 1.0, "the filter")?;
            }
            Self::Tempo { deck, tempo } => {
                deck_index(*deck)?;
                let widest = f32::from(playback::TEMPO_RANGES[1]);
                within(*tempo, -widest, widest, "tempo")?;
            }
            Self::TempoRange { deck, range } => {
                deck_index(*deck)?;
                if !playback::TEMPO_RANGES.contains(range) {
                    return Err(bad("the tempo range is 8 or 16 percent"));
                }
            }
            Self::Crossfader { position } => within(*position, -1.0, 1.0, "the crossfader")?,
            Self::Transition { seconds } => {
                if !(playback::MIN_TRANSITION_SECONDS..=playback::MAX_TRANSITION_SECONDS)
                    .contains(seconds)
                {
                    return Err(bad(format!(
                        "a transition takes {} to {} seconds",
                        playback::MIN_TRANSITION_SECONDS,
                        playback::MAX_TRANSITION_SECONDS
                    )));
                }
            }
            Self::Mode { .. } | Self::Curve { .. } | Self::AutoLoad { .. } => {}
        }
        Ok(())
    }
}

/// A library item a deck takes: a song, album or playlist ID of 1 to
/// [`MAX_DECK_SONG_ID`] bytes.
fn deck_item(id: &str, kind: Option<Kind>) -> Result<(), ApiError> {
    if !matches!(kind, None | Some(Kind::Song | Kind::Album | Kind::Playlist)) {
        return Err(bad("a deck takes songs, albums and playlists"));
    }
    if id.is_empty() || id.len() > MAX_DECK_SONG_ID {
        return Err(bad("invalid song id"));
    }
    Ok(())
}

/// The songs of a library song, album or playlist, in order.
async fn deck_songs(
    bot: &Bot,
    library: usize,
    id: String,
    kind: Option<Kind>,
) -> Result<Vec<Track>, ApiError> {
    ask(bot, library, move |library| match kind {
        Some(Kind::Album) => library.album(&id).map(|(_, songs)| songs),
        Some(Kind::Playlist) => library.playlist(&id).map(|(_, songs)| songs),
        _ => library.song(&id).map(|song| vec![song]),
    })
    .await
}

fn deck_index(deck: usize) -> Result<(), ApiError> {
    if deck < DECKS {
        Ok(())
    } else {
        Err(bad("no such deck"))
    }
}

impl From<DjError> for ApiError {
    fn from(error: DjError) -> Self {
        ApiError(StatusCode::CONFLICT, error.to_string())
    }
}

/// A DJ mode control; the new state reaches every viewer through `events`.
async fn dj(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    // JSON only, as for `command`.
    Json(command): Json<DjCommand>,
) -> Result<StatusCode, ApiError> {
    let owner = owner(&bot, &token)?;
    let player = bot
        .players
        .get(command.player)
        .ok_or_else(|| bad("no such queue"))?;
    command.control.validate()?;
    match command.control {
        DjControl::Mode { on } => player.set_dj(on),
        DjControl::Load {
            deck,
            library: Some(library),
            id: Some(id),
            kind,
            ..
        } => {
            let tracks = deck_songs(&bot, library, id, kind).await?;
            player.dj_load(deck, DeckSong::Library(tracks, owner.name.clone()))?;
        }
        DjControl::Load {
            deck,
            entry: Some(entry),
            ..
        } => player.dj_load(deck, DeckSong::Queued(entry))?,
        DjControl::Load { .. } => return Err(bad("Name a library song or a queue entry.")),
        DjControl::QueueAdd {
            deck,
            library,
            id,
            kind,
            index,
        } => {
            let tracks = deck_songs(&bot, library, id, kind).await?;
            let added = player.dj_queue_add(deck, tracks, &owner.name, index)?;
            tracing::info!(
                "{} added {added} song(s) to a deck queue of {}",
                owner.name,
                player.name
            );
        }
        DjControl::QueueMove { entry, deck, index } => player.dj_queue_move(entry, deck, index)?,
        DjControl::QueueRemove { entry } => player.dj_queue_remove(entry)?,
        DjControl::QueueClear { deck } => player.dj_queue_clear(deck)?,
        DjControl::LoopIn { deck } => player.dj_loop_in(deck)?,
        DjControl::LoopOut { deck } => player.dj_loop_out(deck)?,
        DjControl::LoopToggle { deck } => player.dj_loop_toggle(deck)?,
        DjControl::BeatLoop { deck, beats } => player.dj_beat_loop(deck, beats)?,
        DjControl::LoopResize { deck, factor } => player.dj_loop_resize(deck, factor)?,
        DjControl::LoopMove {
            deck,
            by,
            direction,
        } => player.dj_loop_move(deck, by == LoopStep::Beat, f64::from(direction))?,
        DjControl::LoopSet {
            deck,
            start_ms,
            end_ms,
        } => player.dj_loop_set(
            deck,
            Duration::from_secs_f64(start_ms / 1000.0),
            Duration::from_secs_f64(end_ms / 1000.0),
        )?,
        DjControl::Quantize { deck, on } => player.dj_quantize(deck, on)?,
        DjControl::Eject { deck } => player.dj_eject(deck)?,
        DjControl::Play { deck, playing } => player.dj_play(deck, playing)?,
        DjControl::Seek { deck, position_ms } => {
            player.dj_seek(deck, Duration::from_millis(position_ms))?
        }
        DjControl::SetCue { deck } => player.dj_set_cue(deck)?,
        DjControl::JumpToCue { deck } => player.dj_jump_to_cue(deck)?,
        DjControl::Volume { deck, volume } => player.dj_volume(deck, volume)?,
        DjControl::Eq { deck, band, db } => player.dj_eq(deck, band as usize, db)?,
        DjControl::Filter { deck, value } => player.dj_filter(deck, value)?,
        DjControl::Tempo { deck, tempo } => player.dj_tempo(deck, tempo / 100.0)?,
        DjControl::TempoRange { deck, range } => player.dj_tempo_range(deck, range)?,
        DjControl::Sync { deck } => player.dj_sync(deck)?,
        DjControl::Crossfader { position } => player.dj_crossfader(position)?,
        DjControl::Curve { curve } => player.dj_curve(curve)?,
        DjControl::Transition { seconds } => player.dj_transition(seconds)?,
        DjControl::AutoLoad { on } => player.dj_auto_load(on)?,
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Similar songs, each recording once: the seed, matches with a similarity of
/// 100% and matches with the seed's title and artist are left out, and of
/// several copies of one recording (the same song on another album) the
/// first, closest match stays.
fn distinct_similar(matches: Vec<SonicMatch>, seed: &Track) -> Vec<SonicMatch> {
    let mut kept: Vec<SonicMatch> = Vec::with_capacity(matches.len());
    for found in matches {
        let identical = found.track.id == seed.id
            || found.similarity.is_some_and(|score| score >= 1.0 - 1e-6)
            || same_recording(&found.track, seed)
            || kept
                .iter()
                .any(|earlier| same_recording(&found.track, &earlier.track));
        if !identical {
            kept.push(found);
        }
    }
    kept
}

/// Whether two tracks are copies of one recording: the same title and artist,
/// compared without case or extra spaces, and lengths at most 3 seconds apart
/// when both are known.
fn same_recording(a: &Track, b: &Track) -> bool {
    import::normalized(&a.title) == import::normalized(&b.title)
        && import::normalized(a.artist.as_deref().unwrap_or_default())
            == import::normalized(b.artist.as_deref().unwrap_or_default())
        && match (a.duration, b.duration) {
            (Some(a), Some(b)) => a.abs_diff(b) <= 3,
            _ => true,
        }
}

/// A path that starts with the song the queue is playing continues it: the
/// first song is not queued again. `playing` is the current song's library
/// identity and id.
fn continue_path(
    mut path: Vec<Track>,
    playing: Option<(Option<&LibraryIdentity>, &str)>,
    library: &LibraryIdentity,
) -> Vec<Track> {
    if let (Some((Some(source), id)), Some(first)) = (playing, path.first())
        && source == library
        && id == first.id
    {
        path.remove(0);
    }
    path
}

#[derive(Deserialize)]
struct LibraryQuery {
    #[serde(default)]
    library: usize,
}

#[derive(Deserialize)]
struct ItemQuery {
    #[serde(default)]
    library: usize,
    id: String,
}

#[derive(Serialize)]
struct RatingView {
    rating: u8,
}

async fn rating(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<RatingView>, ApiError> {
    owner(&bot, &token)?;
    let rating = ask(&bot, query.library, move |library| {
        library.rating(&query.id)
    })
    .await?;
    Ok(Json(RatingView { rating }))
}

#[derive(Deserialize)]
struct RatingMutation {
    library: usize,
    id: String,
    rating: i64,
}

async fn set_rating(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    // JSON only, just like queue commands: cross-origin forms cannot submit it.
    Json(mutation): Json<RatingMutation>,
) -> Result<StatusCode, ApiError> {
    owner(&bot, &token)?;
    let rating = u8::try_from(mutation.rating)
        .ok()
        .filter(|rating| *rating <= 5)
        .ok_or_else(|| bad("rating must be between 0 and 5"))?;
    let changed = bot.clone();
    ask(&bot, mutation.library, move |library| {
        library.set_rating(&mutation.id, rating)?;
        // The admitted worker owns this notification: a cancelled or timed-out
        // HTTP caller cannot hide a backend write that actually succeeded.
        changed.rating_revision.fetch_add(1, Ordering::SeqCst);
        changed.changes.notify();
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AlbumsQuery {
    #[serde(default)]
    library: usize,
    #[serde(default)]
    list: String,
    #[serde(default)]
    page: u32,
}

#[derive(Serialize)]
struct AlbumPage {
    albums: Vec<Album>,
    more: bool,
}

async fn albums(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<AlbumsQuery>,
) -> Result<Json<AlbumPage>, ApiError> {
    owner(&bot, &token)?;
    let list = AlbumList::parse(&query.list).unwrap_or(AlbumList::Newest);
    let page = query.page.min(MAX_PAGE);
    let albums = ask(&bot, query.library, move |library| {
        library.album_list(list, PAGE_SIZE, page * PAGE_SIZE)
    })
    .await?;
    // A random list is new every time; "more" would repeat albums.
    let more = list != AlbumList::Random && albums.len() as u32 == PAGE_SIZE;
    Ok(Json(AlbumPage { albums, more }))
}

#[derive(Serialize)]
struct AlbumView {
    album: Album,
    songs: Vec<Track>,
}

async fn album(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<AlbumView>, ApiError> {
    owner(&bot, &token)?;
    let (album, songs) = ask(&bot, query.library, move |library| library.album(&query.id)).await?;
    Ok(Json(AlbumView { album, songs }))
}

#[derive(Deserialize)]
struct GenreQuery {
    #[serde(default)]
    library: usize,
    #[serde(default)]
    name: String,
    #[serde(default)]
    page: u32,
}

/// A genre name the bot asks a library for: 1 to [`MAX_GENRE_NAME`] characters.
fn genre_name(name: String) -> Result<String, ApiError> {
    if name.is_empty() {
        Err(bad("a genre needs a name"))
    } else if name.chars().count() > MAX_GENRE_NAME {
        Err(bad(format!(
            "genre names have at most {MAX_GENRE_NAME} characters"
        )))
    } else {
        Ok(name)
    }
}

/// The library's genres by name with their album and song counts.
async fn genres(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<LibraryQuery>,
) -> Result<Json<Genres>, ApiError> {
    owner(&bot, &token)?;
    Ok(Json(
        ask(&bot, query.library, |library| library.genres()).await?,
    ))
}

/// One page of a genre's albums, [`PAGE_SIZE`] at a time.
async fn genre_albums(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<GenreQuery>,
) -> Result<Json<AlbumPage>, ApiError> {
    owner(&bot, &token)?;
    let name = genre_name(query.name)?;
    let page = query.page.min(MAX_PAGE);
    let albums = ask(&bot, query.library, move |library| {
        library.genre_albums(&name, PAGE_SIZE, page * PAGE_SIZE)
    })
    .await?;
    let more = albums.len() as u32 == PAGE_SIZE;
    Ok(Json(AlbumPage { albums, more }))
}

#[derive(Serialize)]
struct SongPage {
    songs: Vec<Track>,
    more: bool,
}

/// One page of a genre's songs, [`GENRE_SONGS_PAGE`] at a time.
async fn genre_songs(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<GenreQuery>,
) -> Result<Json<SongPage>, ApiError> {
    owner(&bot, &token)?;
    let name = genre_name(query.name)?;
    let page = query.page.min(MAX_PAGE);
    let songs = ask(&bot, query.library, move |library| {
        library.genre_songs(&name, GENRE_SONGS_PAGE, page * GENRE_SONGS_PAGE)
    })
    .await?;
    let more = songs.len() as u32 == GENRE_SONGS_PAGE;
    Ok(Json(SongPage { songs, more }))
}

#[derive(Serialize)]
struct ArtistGroup {
    name: String,
    artists: Vec<Artist>,
}

#[derive(Serialize)]
struct ArtistIndex {
    groups: Vec<ArtistGroup>,
}

async fn artists(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<LibraryQuery>,
) -> Result<Json<ArtistIndex>, ApiError> {
    owner(&bot, &token)?;
    let groups = ask(&bot, query.library, |library| library.artists()).await?;
    Ok(Json(ArtistIndex {
        groups: groups
            .into_iter()
            .map(|(name, artists)| ArtistGroup { name, artists })
            .collect(),
    }))
}

#[derive(Serialize)]
struct ArtistView {
    artist: Artist,
    albums: Vec<Album>,
}

async fn artist(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<ArtistView>, ApiError> {
    owner(&bot, &token)?;
    let (artist, albums) = ask(&bot, query.library, move |library| {
        library.artist(&query.id)
    })
    .await?;
    Ok(Json(ArtistView { artist, albums }))
}

#[derive(Serialize)]
struct SongView {
    song: Track,
    details: SongDetails,
    appearances: Vec<Track>,
    /// The song still shows when searching for its albums fails.
    appearances_error: Option<String>,
}

/// A song's page: its details and the albums it is on.
async fn song(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<SongView>, ApiError> {
    owner(&bot, &token)?;
    let view = ask(&bot, query.library, move |library| {
        let (song, details) = library.song_details(&query.id)?;
        let (appearances, appearances_error) = match library.appearances(&song) {
            Ok(appearances) => (appearances, None),
            Err(error) => (Vec::new(), Some(error.public_message())),
        };
        Ok(SongView {
            song,
            details,
            appearances,
            appearances_error,
        })
    })
    .await?;
    Ok(Json(view))
}

#[derive(Serialize)]
struct LyricsView {
    lyrics: Option<Lyrics>,
}

/// A song's lyrics, null when the library has none.
async fn lyrics(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<LyricsView>, ApiError> {
    owner(&bot, &token)?;
    let lyrics = ask(&bot, query.library, move |library| {
        library.lyrics(&query.id)
    })
    .await?;
    Ok(Json(LyricsView { lyrics }))
}

#[derive(Serialize)]
struct SimilarView {
    song: Track,
    matches: Vec<SonicMatch>,
}

/// The songs that sound like one song, and that song for the heading.
async fn similar(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<SimilarView>, ApiError> {
    owner(&bot, &token)?;
    let (song, matches) = ask(&bot, query.library, move |library| {
        // The extension check comes first: unsupported libraries get no other call.
        let matches = library.sonic_similar(&query.id)?;
        Ok((library.song(&query.id)?, matches))
    })
    .await?;
    let matches = distinct_similar(matches, &song);
    Ok(Json(SimilarView { song, matches }))
}

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    library: usize,
    from: String,
    to: String,
}

#[derive(Serialize)]
struct SonicPathView {
    matches: Vec<SonicMatch>,
}

async fn sonic_path(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<PathQuery>,
) -> Result<Json<SonicPathView>, ApiError> {
    owner(&bot, &token)?;
    let matches = ask(&bot, query.library, move |library| {
        library.sonic_path(&query.from, &query.to)
    })
    .await?;
    Ok(Json(SonicPathView { matches }))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
struct SearchResults {
    results: Vec<LibraryResults>,
}

#[derive(Serialize)]
struct LibraryResults {
    library: usize,
    name: String,
    songs: Vec<Track>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    error: Option<String>,
}

/// Runs `call` on every library with at most eight calls in flight and answers
/// in library order. Overload fails the whole request at once; other failures
/// stay with their library.
async fn ask_every<T: Send + 'static>(
    bot: &Bot,
    call: impl Fn(&Library) -> Result<T, subsonic::Error> + Clone + Send + 'static,
) -> Result<Vec<(usize, Result<T, ApiError>)>, ApiError> {
    let calls = futures_util::stream::iter(0..bot.libraries.len())
        .map(|library| {
            let call = call.clone();
            async move {
                match ask(bot, library, call).await {
                    // Reject overload promptly instead of waiting for the other
                    // calls; their permits stay with their blocking workers.
                    Err(error) if error.0 == StatusCode::SERVICE_UNAVAILABLE => Err(error),
                    result => Ok((library, result)),
                }
            }
        })
        .buffer_unordered(MAX_LIBRARY_CALLS);
    let mut found: Vec<_> = calls.try_collect().await?;
    found.sort_unstable_by_key(|(library, _)| *library);
    Ok(found)
}

/// Searches any number of libraries, with at most eight calls in flight.
async fn search(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResults>, ApiError> {
    owner(&bot, &token)?;
    let text: String = query.q.trim().chars().take(MAX_QUERY).collect();
    if text.is_empty() {
        return Ok(Json(SearchResults {
            results: Vec::new(),
        }));
    }
    let found = ask_every(&bot, move |library| library.search(&text)).await?;
    let mut results = Vec::with_capacity(found.len());
    for (library, result) in found {
        let name = bot.libraries[library].name.clone();
        results.push(match result {
            Ok(found) => LibraryResults {
                library,
                name,
                songs: found.songs,
                albums: found.albums,
                artists: found.artists,
                error: None,
            },
            Err(ApiError(_, error)) => LibraryResults {
                library,
                name,
                songs: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
                error: Some(error),
            },
        });
    }
    Ok(Json(SearchResults { results }))
}

/// Suggestion queries shorter than this, in characters, answer with no items.
const MIN_SUGGEST_QUERY: usize = 2;
/// Most items one suggestion answer holds.
const MAX_SUGGESTIONS: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
enum SuggestionKind {
    Artist,
    Album,
    Song,
}

#[derive(Debug, Serialize)]
struct Suggestion {
    library: usize,
    kind: SuggestionKind,
    id: String,
    title: String,
    /// The artist of an album or song.
    subtitle: Option<String>,
    cover_art: Option<String>,
}

#[derive(Serialize)]
struct Suggestions {
    items: Vec<Suggestion>,
}

/// A few artists, albums and songs for the text typed so far: artists of every
/// library first, then albums, then songs, cut to `MAX_SUGGESTIONS`. An item
/// with the kind, title and artist of an earlier one is left out, so the same
/// artist in two libraries or the same song on two albums shows once. Libraries
/// that fail add nothing.
async fn suggest(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Suggestions>, ApiError> {
    owner(&bot, &token)?;
    let text: String = query.q.trim().chars().take(MAX_QUERY).collect();
    if text.chars().count() < MIN_SUGGEST_QUERY {
        return Ok(Json(Suggestions { items: Vec::new() }));
    }
    let found: Vec<_> = ask_every(&bot, move |library| library.suggest(&text))
        .await?
        .into_iter()
        .filter_map(|(_, result)| result.ok())
        .collect();
    let artists = found
        .iter()
        .flat_map(|found| &found.artists)
        .map(|artist| Suggestion {
            library: artist.library,
            kind: SuggestionKind::Artist,
            id: artist.id.clone(),
            title: artist.name.clone(),
            subtitle: None,
            cover_art: artist.cover_art.clone(),
        });
    let albums = found
        .iter()
        .flat_map(|found| &found.albums)
        .map(|album| Suggestion {
            library: album.library,
            kind: SuggestionKind::Album,
            id: album.id.clone(),
            title: album.name.clone(),
            subtitle: album.artist.clone(),
            cover_art: album.cover_art.clone(),
        });
    let songs = found
        .iter()
        .flat_map(|found| &found.songs)
        .map(|song| Suggestion {
            library: song.library,
            kind: SuggestionKind::Song,
            id: song.id.clone(),
            title: song.title.clone(),
            subtitle: song.artist.clone(),
            cover_art: song.cover_art.clone(),
        });
    let mut seen = std::collections::HashSet::new();
    Ok(Json(Suggestions {
        items: artists
            .chain(albums)
            .chain(songs)
            .filter(|item| {
                seen.insert((
                    item.kind,
                    import::normalized(&item.title),
                    import::normalized(item.subtitle.as_deref().unwrap_or_default()),
                ))
            })
            .take(MAX_SUGGESTIONS)
            .collect(),
    }))
}

/// Adding music, or 404 where it is not configured.
fn adding(bot: &Bot) -> Result<Arc<import::Adding>, ApiError> {
    bot.adding.clone().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            import::Failure::Unavailable.message().to_owned(),
        )
    })
}

fn import_error(failure: import::Failure) -> ApiError {
    ApiError(
        StatusCode::from_u16(failure.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        failure.message().to_owned(),
    )
}

/// Runs an import service call under its own admission and deadline.
async fn import_call<T: Send + 'static>(
    adding: &import::Adding,
    call: impl FnOnce(&import::Client) -> Result<T, import::Failure> + Send + 'static,
) -> Result<T, ApiError> {
    let client = adding.client.clone();
    admitted_call(&import::IMPORT_CALLS, import::TIMEOUT, move || {
        call(&client)
    })
    .await
    .map_err(|failure| {
        import_error(match failure {
            CallFailure::Busy => import::Failure::Busy,
            CallFailure::Failed(failure) => failure,
            CallFailure::Crashed | CallFailure::TimedOut => import::Failure::Unavailable,
        })
    })
}

#[derive(Serialize)]
struct ImportAlbums {
    albums: Vec<import::Album>,
}

async fn import_search(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<ImportAlbums>, ApiError> {
    let owner = owner(&bot, &token)?;
    let adding = adding(&bot)?;
    let text: String = query.q.trim().chars().take(MAX_QUERY).collect();
    if text.is_empty() {
        return Ok(Json(ImportAlbums { albums: Vec::new() }));
    }
    let requester = bot.pseudonyms.of(&owner);
    let albums = import_call(&adding, move |client| client.search(&requester, &text)).await?;
    Ok(Json(ImportAlbums { albums }))
}

#[derive(Deserialize)]
struct ImportIdQuery {
    #[serde(default)]
    id: String,
}

#[derive(Serialize)]
struct InLibrary {
    library: usize,
    id: String,
}

#[derive(Serialize)]
struct ImportAlbumView {
    album: import::AlbumDetails,
    in_library: Option<InLibrary>,
}

async fn import_album(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ImportIdQuery>,
) -> Result<Json<ImportAlbumView>, ApiError> {
    let owner = owner(&bot, &token)?;
    let adding = adding(&bot)?;
    if !import::valid_id(&query.id) {
        return Err(import_error(import::Failure::NotAvailable));
    }
    let requester = bot.pseudonyms.of(&owner);
    let id = query.id;
    let album = import_call(&adding, move |client| client.album(&requester, &id)).await?;
    let in_library = in_library(&bot, adding.library, &album.album).await;
    Ok(Json(ImportAlbumView { album, in_library }))
}

/// An album of the same title and artist in the library added albums arrive
/// in, from one search there; `None` also when that search fails.
async fn in_library(bot: &Bot, library: usize, album: &import::Album) -> Option<InLibrary> {
    let text: String = format!("{} {}", album.artist, album.title)
        .trim()
        .chars()
        .take(MAX_QUERY)
        .collect();
    let found = ask(bot, library, move |library| library.search(&text))
        .await
        .ok()?;
    let title = import::normalized(&album.title);
    let artist = import::normalized(&album.artist);
    found
        .albums
        .into_iter()
        .find(|candidate| {
            import::normalized(&candidate.name) == title
                && candidate
                    .artist
                    .as_deref()
                    .is_some_and(|name| import::normalized(name) == artist)
        })
        .map(|candidate| InLibrary {
            library,
            id: candidate.id,
        })
}

#[derive(Serialize)]
struct BudgetView {
    mine: import::Budget,
    total: import::Budget,
}

#[derive(Serialize)]
struct ImportRequests {
    requests: Vec<import::Import>,
    budget: BudgetView,
}

async fn import_requests(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
) -> Result<Json<ImportRequests>, ApiError> {
    let owner = owner(&bot, &token)?;
    let adding = adding(&bot)?;
    let requester = bot.pseudonyms.of(&owner);
    let list = import_call(&adding, move |client| client.imports(&requester)).await?;
    Ok(Json(ImportRequests {
        requests: list.imports,
        budget: BudgetView {
            mine: list.budget.requester,
            total: list.budget.total,
        },
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewImportRequest {
    album_id: String,
}

#[derive(Serialize)]
struct ImportRequestView {
    request: import::Import,
    existing: bool,
}

async fn import_request(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    // JSON only, like the other POSTs: cross-origin forms cannot send it.
    Json(body): Json<NewImportRequest>,
) -> Result<Json<ImportRequestView>, ApiError> {
    let owner = owner(&bot, &token)?;
    let adding = adding(&bot)?;
    if !import::valid_id(&body.album_id) {
        return Err(import_error(import::Failure::NotAvailable));
    }
    let requester = bot.pseudonyms.of(&owner);
    let album_id = body.album_id;
    let (request, existing) =
        import_call(&adding, move |client| client.request(&requester, &album_id)).await?;
    adding.requested();
    Ok(Json(ImportRequestView { request, existing }))
}

#[derive(Deserialize)]
struct ImportArtQuery {
    #[serde(default = "default_art_size")]
    size: u32,
}

/// An import service cover as a PNG the content worker re-encoded, cached like
/// library covers under [`IMPORT_NAMESPACE`].
async fn import_art(
    State(bot): State<Arc<Bot>>,
    Path((token, id)): Path<(String, String)>,
    Query(query): Query<ImportArtQuery>,
) -> Result<Response, ApiError> {
    owner(&bot, &token)?;
    let adding = adding(&bot)?;
    if !import::valid_id(&id) {
        return Err(import_error(import::Failure::NotAvailable));
    }
    let size = snap_cover_size(query.size);
    let key: CoverKey = (IMPORT_NAMESPACE.to_owned(), id.clone(), size);
    let body = match bot.covers.get(&key).await {
        Some(body) => body,
        None => {
            // The cache is shared, so the bot asks: no viewer's lookups are spent.
            let image = move |fetch: SemaphorePermit<'static>| async move {
                import_call(&adding, move |client| {
                    let _fetch = fetch;
                    client.cover(import::BOT_REQUESTER, &id)
                })
                .await
            };
            fetch_cover(&bot, key, image, worker_thumbnail).await?
        }
    };
    owner(&bot, &token)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            // Import service ids name one album for good.
            (header::CACHE_CONTROL, "private, max-age=86400"),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_redirect_is_canonical_and_cannot_escape_the_origin() {
        for token in ["//evil.example", "/evil", "https://evil.example", "bad"] {
            let response = slash(Path(token.into()))
                .await
                .err()
                .unwrap()
                .into_response();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        let response = slash(Path("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".into()))
            .await
            .ok()
            .expect("valid UUID redirect")
            .into_response();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/"
        );
    }

    #[tokio::test]
    async fn the_page_runs_at_the_root_with_a_remembered_link_never_in_the_address() {
        let (directory, mut bot, token) = test_bot("http://unused.invalid", 0);
        std::fs::write(directory.path().join("index.html"), "<!DOCTYPE html><p>app").unwrap();
        Arc::get_mut(&mut bot).unwrap().web_assets = Some(directory.path().to_owned());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(bot.clone());
        let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // (status, Location, Set-Cookie, body) without following redirects.
        async fn get(
            url: String,
            cookie: Option<String>,
            body: Option<&'static str>,
        ) -> (u16, Option<String>, Option<String>, String) {
            tokio::task::spawn_blocking(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .max_redirects(0)
                    .timeout_global(Some(Duration::from_secs(2)))
                    .build()
                    .into();
                let mut response = match body {
                    Some(body) => {
                        let mut request =
                            agent.post(&url).header("Content-Type", "application/json");
                        if let Some(cookie) = &cookie {
                            request = request.header("Cookie", cookie);
                        }
                        request.send(body).unwrap()
                    }
                    None => {
                        let mut request = agent.get(&url);
                        if let Some(cookie) = &cookie {
                            request = request.header("Cookie", cookie);
                        }
                        request.call().unwrap()
                    }
                };
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .map(|value| value.to_str().unwrap().to_owned())
                };
                let (location, set_cookie) = (header("location"), header("set-cookie"));
                let status = response.status().as_u16();
                let body = response.body_mut().read_to_string().unwrap();
                (status, location, set_cookie, body)
            })
            .await
            .unwrap()
        }
        let cookie = format!("other=1; {LINK_COOKIE}={token}; more=2");

        // The `!web` link only remembers itself and leaves for the root.
        let (status, location, set_cookie, _) = get(format!("{base}/u/{token}/"), None, None).await;
        assert_eq!((status, location.as_deref()), (303, Some("../../")));
        let set_cookie = set_cookie.unwrap();
        assert!(
            set_cookie.starts_with(&format!("{LINK_COOKIE}={token}; Path=/; ")),
            "{set_cookie}"
        );
        for attribute in ["HttpOnly", "SameSite=Lax", "Max-Age=7776000"] {
            assert!(set_cookie.contains(attribute), "{set_cookie}");
        }
        assert!(!set_cookie.contains("Secure"), "plain HTTP public URL");

        // The root shows the page only to the remembering browser.
        let (status, _, _, page) = get(format!("{base}/"), Some(cookie.clone()), None).await;
        assert_eq!((status, page.as_str()), (200, "<!DOCTYPE html><p>app"));
        let (status, _, set_cookie, page) = get(format!("{base}/"), None, None).await;
        assert!(status == 200 && page.contains("!web") && set_cookie.is_none());

        // Token-free page requests use the cookie; without it they are unknown.
        let (status, _, _, state) =
            get(format!("{base}/api/state"), Some(cookie.clone()), None).await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&state).unwrap()["you"],
            "Listener"
        );
        for missing in [None, Some(format!("{LINK_COOKIE}=not-a-uuid"))] {
            let (status, _, _, body) = get(format!("{base}/api/state"), missing, None).await;
            assert!(
                status == 404 && body.contains(INVALID_LINK),
                "{status} {body}"
            );
        }
        // Commands reach the handler (no queues here) only with the cookie.
        let command = r#"{"command":"pause","player":0}"#;
        assert_eq!(
            get(
                format!("{base}/api/command"),
                Some(cookie.clone()),
                Some(command)
            )
            .await
            .0,
            400
        );
        assert_eq!(
            get(format!("{base}/api/command"), None, Some(command))
                .await
                .0,
            404
        );
        // An explicit link in the path never falls back to the cookie.
        let nil = Uuid::nil();
        assert_eq!(
            get(
                format!("{base}/u/{nil}/api/state"),
                Some(cookie.clone()),
                None
            )
            .await
            .0,
            404
        );

        // Page requests from another origin or site cannot use the cookie,
        // for reading as well as for changes. The public origin is
        // http://localhost; a default port is the same origin.
        async fn status_with(
            url: String,
            headers: Vec<(&'static str, String)>,
            body: Option<&'static str>,
        ) -> u16 {
            tokio::task::spawn_blocking(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .timeout_global(Some(Duration::from_secs(2)))
                    .build()
                    .into();
                match body {
                    Some(body) => {
                        let mut request =
                            agent.post(&url).header("Content-Type", "application/json");
                        for (name, value) in &headers {
                            request = request.header(*name, value);
                        }
                        request.send(body).unwrap().status().as_u16()
                    }
                    None => {
                        let mut request = agent.get(&url);
                        for (name, value) in &headers {
                            request = request.header(*name, value);
                        }
                        request.call().unwrap().status().as_u16()
                    }
                }
            })
            .await
            .unwrap()
        }
        let from = |extra: &[(&'static str, &str)]| {
            let mut headers = vec![("Cookie", cookie.clone())];
            headers.extend(
                extra
                    .iter()
                    .map(|(name, value)| (*name, (*value).to_owned())),
            );
            headers
        };
        for refused in [
            &[("Origin", "https://evil.example")][..],
            &[("Origin", "http://localhost:8080")],
            &[("Origin", "null")],
            &[("Sec-Fetch-Site", "cross-site")],
            &[("Sec-Fetch-Site", "same-site")],
            &[
                ("Origin", "http://localhost"),
                ("Sec-Fetch-Site", "same-site"),
            ],
        ] {
            for (endpoint, body) in [
                ("api/command", Some(command)),
                ("api/state", None),
                ("api/events", None),
                ("api/listen?player=0", None),
                ("api/rating", Some(r#"{"library":0,"id":"s","rating":5}"#)),
                ("art/0/cover?size=32", None),
                ("import-art/00000000000000a1?size=32", None),
                ("api/forget", Some("{}")),
            ] {
                assert_eq!(
                    status_with(format!("{base}/{endpoint}"), from(refused), body).await,
                    403,
                    "{endpoint} {refused:?}"
                );
            }
        }
        for allowed in [
            &[
                ("Origin", "http://LOCALHOST:80"),
                ("Sec-Fetch-Site", "same-origin"),
            ][..],
            &[("Sec-Fetch-Site", "none")],
        ] {
            assert_eq!(
                status_with(format!("{base}/api/command"), from(allowed), Some(command)).await,
                400,
                "{allowed:?}"
            );
        }
        // Scripts with the link in the path are not page requests.
        assert_eq!(
            status_with(
                format!("{base}/u/{token}/api/state"),
                vec![("Origin", "https://evil.example".to_owned())],
                None
            )
            .await,
            200
        );
        // Forgetting clears the cookie and needs JSON.
        let (status, _, set_cookie, _) = get(
            format!("{base}/api/forget"),
            Some(cookie.clone()),
            Some("{}"),
        )
        .await;
        assert_eq!(status, 204);
        assert!(
            set_cookie
                .unwrap()
                .starts_with(&format!("{LINK_COOKIE}=; Path=/; Max-Age=0"))
        );
        assert_eq!(
            status_with(
                format!("{base}/api/forget"),
                vec![("Cookie", cookie.clone())],
                None
            )
            .await,
            405,
            "a GET does nothing"
        );

        // Renewal revokes the remembered link: the root forgets it.
        let viewer = owner(&bot, &token).ok().unwrap();
        bot.tokens.issue(viewer, true).unwrap();
        let (status, _, set_cookie, page) =
            get(format!("{base}/"), Some(cookie.clone()), None).await;
        assert!(status == 200 && page.contains("!web"));
        let set_cookie = set_cookie.unwrap();
        assert!(
            set_cookie.starts_with(&format!("{LINK_COOKIE}=; Path=/; Max-Age=0")),
            "{set_cookie}"
        );
        assert_eq!(
            get(format!("{base}/api/state"), Some(cookie), None).await.0,
            404
        );
        let (status, _, set_cookie, _) = get(format!("{base}/u/{token}/"), None, None).await;
        assert!(status == 404 && set_cookie.is_none());
        serving.abort();
        let _ = serving.await;
    }

    #[test]
    fn the_remembered_link_stays_below_the_public_path_with_a_prefixed_name() {
        let token = Uuid::from_u128(7);
        for (public, name, path, secure, origin) in [
            (
                "https://music.example.org",
                "__Host-musicbot_link",
                "/",
                true,
                "https://music.example.org",
            ),
            (
                "https://Music.example.org:443/bot",
                "__Secure-musicbot_link",
                "/bot/",
                true,
                "https://music.example.org",
            ),
            (
                "HTTPS://music.example.org:8443/bot/",
                "__Secure-musicbot_link",
                "/bot/",
                true,
                "https://music.example.org:8443",
            ),
            (
                "http://music.example.org",
                "musicbot_link",
                "/",
                false,
                "http://music.example.org",
            ),
            (
                "https://music.example.org/a;b",
                "__Host-musicbot_link",
                "/",
                true,
                "https://music.example.org",
            ),
        ] {
            let site = Site::new(public);
            assert_eq!(site.origin.as_deref(), Some(origin), "{public}");
            let cookie = link_cookie(&site, Some(&token))
                .to_str()
                .unwrap()
                .to_owned();
            assert!(
                cookie.starts_with(&format!("{name}={token}; Path={path}; ")),
                "{public}: {cookie}"
            );
            assert_eq!(cookie.ends_with("; Secure"), secure, "{public}: {cookie}");
            let mut headers = HeaderMap::new();
            let cookies = format!("other=x; {name}={token}");
            headers.insert(header::COOKIE, cookies.parse().unwrap());
            assert_eq!(remembered_link(&headers, &site), Some(Some(token)));
        }
    }

    #[test]
    fn covers_are_made_in_a_few_sizes_never_larger_than_asked() {
        let snapped: Vec<_> = [0, 31, 32, 63, 96, 127, 250, 300, 399, 400, 4000]
            .into_iter()
            .map(snap_cover_size)
            .collect();
        assert_eq!(snapped, [32, 32, 32, 32, 96, 96, 200, 300, 300, 400, 400]);
    }
    use subsonic::tests::Server;

    // These API tests share the production admission pool; the overload test
    // deliberately occupies every permit and must not race other API consumers.
    pub(super) static API_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(super) fn test_bot(
        url: &str,
        library_count: usize,
    ) -> (tempfile::TempDir, Arc<Bot>, String) {
        let directory = tempfile::tempdir().unwrap();
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "web": { "listen": "127.0.0.1:0", "public_url": "http://localhost/" },
            "subsonic": (0..library_count).map(|index| serde_json::json!({
                "name": format!("Library {index}"), "url": url,
            })).collect::<Vec<_>>(),
            "channel": [],
        }))
        .unwrap();
        let identities: Vec<_> = config
            .libraries
            .iter()
            .map(crate::subsonic::LibraryIdentity::new)
            .collect();
        let keys = crate::library_keys::load(directory.path(), &identities).unwrap();
        let libraries = config
            .libraries
            .iter()
            .zip(keys.iter().cloned())
            .enumerate()
            .map(|(index, (library, key))| {
                Library::new(
                    index,
                    library,
                    crate::config::Credentials::ApiKey("private-key".into()),
                    key,
                )
            })
            .collect();
        let tokens = crate::tokens::Tokens::load(directory.path().join("tokens.json")).unwrap();
        let token = tokens
            .issue(
                Owner {
                    server: "mumble.example:64738".into(),
                    name: "Listener".into(),
                    certificate: Some("certificate".into()),
                },
                false,
            )
            .unwrap()
            .to_string();
        let bot = Arc::new(Bot {
            config,
            state_directory: directory.path().to_owned(),
            web_assets: None,
            codec_worker: std::path::PathBuf::from("/nonexistent/nuschl-musicbot-codec-worker"),
            preload: None,
            libraries: Arc::new(libraries),
            players: Vec::new(),
            connections: Vec::new(),
            tokens,
            changes: crate::Changes::default(),
            persistence: std::sync::Mutex::new(()),
            covers: crate::covers::CoverCache::open(directory.path().join("covers"), 1 << 20)
                .unwrap(),
            adding: None,
            rating_revision: std::sync::atomic::AtomicU64::new(0),
            playlists: crate::playlists::Playlists::load(
                directory.path().join(crate::playlists::FILE),
                &keys,
            )
            .unwrap(),
            pseudonyms: import::Pseudonyms::load(directory.path()).unwrap(),
        });
        (directory, bot, token)
    }

    fn listening_bot() -> (tempfile::TempDir, Arc<Bot>, String) {
        let (directory, mut bot, token) = test_bot("http://unused.invalid", 0);
        let unique = Arc::get_mut(&mut bot).unwrap();
        unique.players.push(Arc::new(crate::player::Player::new(
            "Browser queue".into(),
            50,
            Ducking::default(),
            unique.changes.clone(),
        )));
        (directory, bot, token)
    }

    async fn released(bot: &Bot, count: usize) {
        let mut changes = bot.changes.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            while bot.players[0].listeners() != count {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn listening_validates_links_and_queues_before_admission_and_bounds_live_bodies() {
        static ADMISSION: Semaphore = Semaphore::const_new(MAX_LISTEN_STREAMS);
        let (_directory, bot, token) = listening_bot();
        assert_eq!(
            listen_response(bot.clone(), "invalid".into(), 0, Listen::Room, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            listen_response(bot.clone(), token.clone(), 1, Listen::Room, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(ADMISSION.available_permits(), MAX_LISTEN_STREAMS);
        let mut responses = Vec::new();
        for _ in 0..MAX_LISTEN_STREAMS {
            responses.push(
                listen_response(bot.clone(), token.clone(), 0, Listen::Room, &ADMISSION)
                    .ok()
                    .unwrap(),
            );
        }
        assert_eq!(bot.players[0].listeners(), MAX_LISTEN_STREAMS);
        assert_eq!(ADMISSION.available_permits(), 0);
        assert_eq!(
            listen_response(bot.clone(), token.clone(), 0, Listen::Room, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            listen_response(bot.clone(), "invalid".into(), 0, Listen::Room, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::NOT_FOUND,
            "auth precedes overload"
        );
        assert_eq!(
            listen_response(bot.clone(), token.clone(), 1, Listen::Room, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::BAD_REQUEST,
            "queue validation precedes overload"
        );
        drop(responses.pop());
        released(&bot, MAX_LISTEN_STREAMS - 1).await;
        assert_eq!(ADMISSION.available_permits(), 1);
        responses.push(
            listen_response(bot.clone(), token, 0, Listen::Room, &ADMISSION)
                .ok()
                .unwrap(),
        );
        drop(responses);
        released(&bot, 0).await;
        assert_eq!(ADMISSION.available_permits(), MAX_LISTEN_STREAMS);
    }

    #[tokio::test]
    async fn listening_emits_initial_reset_and_keeps_ownership_until_body_disconnects() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        let response = listen_response(bot.clone(), token, 0, Listen::Room, &ADMISSION)
            .ok()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for (name, value) in [
            ("content-type", "application/octet-stream"),
            ("cache-control", "no-store"),
            ("x-accel-buffering", "no"),
            ("x-audio-format", "f32le"),
            ("x-audio-rate", "48000"),
            ("x-audio-channels", "2"),
            ("x-audio-frame-samples", "960"),
        ] {
            assert_eq!(response.headers()[name], value);
        }
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), &[0]);
        assert_eq!(
            ADMISSION.available_permits(),
            0,
            "headers do not release admission"
        );
        assert_eq!(bot.players[0].listeners(), 1);
        drop(stream);
        released(&bot, 0).await;
        assert_eq!(ADMISSION.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn revoked_idle_unpolled_listening_is_reaped_within_one_second() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        bot.players[0].set_paused(true);
        let response = listen_response(bot.clone(), token.clone(), 0, Listen::Room, &ADMISSION)
            .ok()
            .unwrap();
        tokio::task::yield_now().await;
        let viewer = owner(&bot, &token).ok().unwrap();
        bot.tokens.issue(viewer, true).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        released(&bot, 0).await;
        assert_eq!(
            ADMISSION.available_permits(),
            1,
            "revocation drops the real lease even with HTTP backpressure"
        );
        let mut stream = response.into_body().into_data_stream();
        assert!(
            stream.next().await.is_none(),
            "revoked queued records are not delivered"
        );
        assert_eq!(
            listen_response(bot, token, 0, Listen::Room, &ADMISSION)
                .err()
                .unwrap()
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_listening_sends_a_keepalive_record_at_least_every_fifteen_seconds() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        bot.players[0].set_paused(true);
        let response = listen_response(bot.clone(), token, 0, Listen::Room, &ADMISSION)
            .ok()
            .unwrap();
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), &[0]);
        for _ in 0..3 {
            assert!(
                tokio::time::timeout(Duration::from_secs(14), stream.next())
                    .await
                    .is_err(),
                "a keep-alive came early"
            );
            let record = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("no keep-alive within 15 seconds")
                .unwrap()
                .unwrap();
            assert_eq!(record.as_ref(), KEEPALIVE_RECORD);
        }
        assert_eq!(ADMISSION.available_permits(), 0);
        drop(stream);
        released(&bot, 0).await;
        assert_eq!(ADMISSION.available_permits(), 1);
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn real_http_sse_cap_is_retained_until_the_stream_closes() {
        // The rating test also needs an SSE permit while this test fills the pool.
        let _exclusive = API_TESTS.lock().await;
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::{TcpListener, TcpStream},
        };
        async fn open(address: std::net::SocketAddr, token: &str) -> (TcpStream, String) {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket
                .write_all(
                    format!("GET /u/{token}/api/events HTTP/1.1\r\nHost: fixture\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let mut header = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), async {
                while !header.ends_with(b"\r\n\r\n") {
                    assert!(header.len() < 8192);
                    header.push(socket.read_u8().await.unwrap());
                }
            })
            .await
            .unwrap();
            (socket, String::from_utf8(header).unwrap())
        }
        let library = Server::reply("application/json", b"{}");
        let (_directory, bot, token) = test_bot(&library.url, 1);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            axum::serve(listener, router(bot)).await.unwrap();
        });
        let mut clients = Vec::new();
        for _ in 0..MAX_SSE_WATCHERS {
            let (socket, header) = open(address, &token).await;
            assert!(header.starts_with("HTTP/1.1 200"), "{header}");
            clients.push(socket);
        }
        assert_eq!(SSE_WATCHERS.available_permits(), 0);
        let (overload, header) = open(address, &token).await;
        assert!(header.starts_with("HTTP/1.1 503"), "{header}");
        drop(overload);
        // A completed handler did not release a live response body's permit.
        assert_eq!(SSE_WATCHERS.available_permits(), 0);
        drop(clients.pop());
        tokio::time::timeout(Duration::from_secs(5), async {
            while SSE_WATCHERS.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (replacement, header) = open(address, &token).await;
        assert!(header.starts_with("HTTP/1.1 200"), "{header}");
        clients.push(replacement);
        drop(clients);
        tokio::time::timeout(Duration::from_secs(5), async {
            while SSE_WATCHERS.available_permits() != MAX_SSE_WATCHERS {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        serving.abort();
        let _ = serving.await;
    }

    #[tokio::test]
    async fn rating_api_reads_sets_clears_and_only_publishes_successful_writes() {
        use std::{io::Write, sync::Mutex};

        let _exclusive = API_TESTS.lock().await;
        #[derive(Default)]
        struct Account {
            rating: Option<i64>,
            omit_rating: bool,
            fail: bool,
            calls: usize,
            writes: Vec<i64>,
        }
        let account = Arc::new(Mutex::new(Account {
            omit_rating: true,
            ..Account::default()
        }));
        let backend = Server::new({
            let account = account.clone();
            move |request, socket| {
                let target = request.split_whitespace().nth(1).unwrap();
                let (path, query) = target.split_once('?').unwrap();
                let parameter = |name: &str| {
                    query.split('&').find_map(|pair| {
                        let (key, value) = pair.split_once('=')?;
                        (key == name).then_some(value)
                    })
                };
                assert_eq!(parameter("apiKey"), Some("private-key"));
                assert_eq!(parameter("id"), Some("song"));
                let mut state = account.lock().unwrap();
                state.calls += 1;
                let reply = match path {
                    "/rest/getSong" => {
                        let mut song = serde_json::json!({"id": "song"});
                        if !state.omit_rating {
                            song["userRating"] = serde_json::json!(state.rating);
                        }
                        serde_json::json!({"status": "ok", "song": song})
                    }
                    "/rest/setRating" if state.fail => serde_json::json!({
                        "status": "failed", "error": {"code": 40,
                            "message": "http://private.example/?apiKey=private-key"}
                    }),
                    "/rest/setRating" => {
                        let rating = parameter("rating").unwrap().parse::<i64>().unwrap();
                        assert!((0..=5).contains(&rating));
                        state.writes.push(rating);
                        state.rating = (rating != 0).then_some(rating);
                        state.omit_rating = false;
                        serde_json::json!({"status": "ok"})
                    }
                    _ => panic!("unexpected backend endpoint"),
                };
                let body =
                    serde_json::to_vec(&serde_json::json!({"subsonic-response": reply})).unwrap();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let (_directory, bot, token) = test_bot(&backend.url, 1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/u/{token}/api", listener.local_addr().unwrap());
        let app = router(bot.clone());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });

        async fn request(url: String, body: Option<serde_json::Value>) -> (u16, serde_json::Value) {
            tokio::task::spawn_blocking(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .timeout_global(Some(Duration::from_secs(2)))
                    .build()
                    .into();
                let mut response = match body {
                    Some(body) => agent.post(url).send_json(body).unwrap(),
                    None => agent.get(url).call().unwrap(),
                };
                let status = response.status().as_u16();
                let body = response.body_mut().read_to_vec().unwrap();
                (
                    status,
                    if body.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_slice(&body).unwrap()
                    },
                )
            })
            .await
            .unwrap()
        }
        let read_url = format!("{base}/rating?library=0&id=song");
        let write_url = format!("{base}/rating");
        let mutation = |library, rating| serde_json::json!({"library": library, "id": "song", "rating": rating});
        let missing_token = base.replace(&token, &Uuid::nil().to_string());
        assert_eq!(
            request(format!("{missing_token}/rating?library=0&id=song"), None)
                .await
                .0,
            404
        );
        assert_eq!(
            request(format!("{missing_token}/rating"), Some(mutation(0, 4)))
                .await
                .0,
            404
        );
        for invalid in [-1, 6] {
            assert_eq!(
                request(write_url.clone(), Some(mutation(0, invalid)))
                    .await
                    .0,
                400
            );
        }
        assert_eq!(
            request(write_url.clone(), Some(mutation(1, 4))).await.0,
            502
        );
        assert_eq!(
            request(format!("{base}/rating?library=1&id=song"), None)
                .await
                .0,
            502
        );
        assert_eq!(account.lock().unwrap().calls, 0);
        assert_eq!(
            request(read_url.clone(), None).await,
            (200, serde_json::json!({"rating": 0}))
        );
        account.lock().unwrap().omit_rating = false;
        assert_eq!(request(read_url.clone(), None).await.1["rating"], 0);

        // Consume the same SSE body the endpoint sends, checking the emitted
        // revision rather than merely an internal counter.
        let mut stream = events(State(bot.clone()), Path(token.clone()))
            .await
            .ok()
            .unwrap()
            .into_body()
            .into_data_stream();
        async fn event_revision(
            stream: &mut (impl Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
        ) -> u64 {
            let chunk = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let text = std::str::from_utf8(&chunk).unwrap();
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .or_else(|| text.lines().find_map(|line| line.strip_prefix("data:")))
                .unwrap();
            serde_json::from_str::<serde_json::Value>(data).unwrap()["rating_revision"]
                .as_u64()
                .unwrap()
        }
        assert_eq!(event_revision(&mut stream).await, 0);
        for (rating, revision) in [(4, 1), (0, 2)] {
            assert_eq!(
                request(write_url.clone(), Some(mutation(0, rating)))
                    .await
                    .0,
                204
            );
            assert_eq!(request(read_url.clone(), None).await.1["rating"], rating);
            assert_eq!(
                request(format!("{base}/state"), None).await.1["rating_revision"],
                revision
            );
            assert_eq!(event_revision(&mut stream).await, revision as u64);
        }
        account.lock().unwrap().fail = true;
        let failed = request(write_url, Some(mutation(0, 5))).await;
        assert_eq!(failed.0, 502);
        assert!(!failed.1.to_string().contains("private-key"));
        assert_eq!(request(read_url.clone(), None).await.1["rating"], 0);
        assert_eq!(
            request(format!("{base}/state"), None).await.1["rating_revision"],
            2
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err()
        );
        assert_eq!(account.lock().unwrap().writes, [4, 0]);
        for invalid in [-1, 6] {
            account.lock().unwrap().rating = Some(invalid);
            assert_eq!(request(read_url.clone(), None).await.0, 502);
        }
        account.lock().unwrap().rating = Some(3);
        assert_eq!(request(read_url, None).await.1["rating"], 3);
        drop(stream);
        stop.send(()).unwrap();
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_search_and_art_redact_remote_messages_and_reject_overload() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::reply("application/json", br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"http://dj:secret@private.example/rest/search3?apiKey=private-key"}}}"#);
        let (_directory, bot, token) = test_bot(&server.url, 9);
        let response = albums(
            State(bot.clone()),
            Path(token.clone()),
            Query(AlbumsQuery {
                library: 0,
                list: "newest".into(),
                page: 0,
            }),
        )
        .await
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response_json(response).await["error"],
            "the music library rejected the request (error 40)"
        );

        // More than eight configured libraries still all produce ordered,
        // independently redacted results; only actual parallel work is capped.
        let found = search(
            State(bot.clone()),
            Path(token.clone()),
            Query(SearchQuery { q: "song".into() }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!(found.results.len(), 9);
        for (index, result) in found.results.iter().enumerate() {
            assert_eq!(result.library, index);
            assert_eq!(
                result.error.as_deref(),
                Some("the music library rejected the request (error 40)")
            );
        }
        assert!(
            !serde_json::to_string(&found)
                .unwrap()
                .contains("private-key")
        );

        let response = art(
            State(bot.clone()),
            Path((token.clone(), 0, "cover".into())),
            Query(ArtQuery { size: 300, v: None }),
        )
        .await
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_json(response).await["error"],
            "the music library sent a non-image response"
        );

        let occupied = LIBRARY_CALLS
            .acquire_many(MAX_LIBRARY_CALLS as u32)
            .await
            .unwrap();
        // Covers have their own fetch admission and still reach the library.
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            art(
                State(bot.clone()),
                Path((token.clone(), 0, "cover".into())),
                Query(ArtQuery { size: 300, v: None }),
            ),
        )
        .await
        .unwrap()
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_json(response).await["error"],
            "the music library sent a non-image response"
        );
        let response = tokio::time::timeout(
            Duration::from_millis(200),
            search(
                State(bot),
                Path(token),
                Query(SearchQuery { q: "song".into() }),
            ),
        )
        .await
        .unwrap()
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response_json(response).await["error"],
            "the music library is busy"
        );
        drop(occupied);
    }

    #[tokio::test]
    async fn cached_covers_skip_the_library_and_only_versioned_urls_may_be_kept() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::reply(
            "application/json",
            br#"{"subsonic-response":{"status":"failed","error":{"code":0}}}"#,
        );
        let (_directory, bot, token) = test_bot(&server.url, 1);
        let art_key = bot.libraries[0].key().to_owned();
        let viewer = owner(&bot, &token).ok().unwrap();
        assert_eq!(snapshot(&bot, &viewer).library_keys, [art_key.as_str()]);
        // A 1×1 thumbnail PNG.
        let body = Bytes::from_static(b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15\xc4\x89\x00\x00\x00\x0d\x49\x44\x41\x54\x78\x9c\x63\x38\x11\xa0\xf1\x1f\x00\x05\x64\x02\x40\x64\x52\x9c\x3a\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82");
        bot.covers
            .insert(&(art_key.clone(), "cover".into(), 300), &body)
            .await;
        // A hit needs no fetch and no artwork job.
        let fetches = COVER_FETCHES
            .acquire_many(MAX_COVER_FETCHES as u32)
            .await
            .unwrap();
        let jobs = COVER_JOBS
            .acquire_many(MAX_COVER_JOBS as u32)
            .await
            .unwrap();
        let request = |v: Option<&str>| {
            art(
                State(bot.clone()),
                Path((token.clone(), 0, "cover".into())),
                Query(ArtQuery {
                    size: 300,
                    v: v.map(str::to_owned),
                }),
            )
        };
        for (v, caching) in [
            (Some(art_key.as_str()), "private, max-age=86400"),
            (None, "private, no-store"),
        ] {
            let response = request(v).await.ok().unwrap();
            assert_eq!(response.headers()[header::CACHE_CONTROL], caching);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
            let served = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(served, body);
        }
        let stale = request(Some("0000000000000000")).await.err().unwrap();
        assert_eq!(stale.0, StatusCode::NOT_FOUND);
        let other = |bot: Arc<Bot>, token: String| {
            art(
                State(bot),
                Path((token, 0, "other".into())),
                Query(ArtQuery { size: 300, v: None }),
            )
        };
        // Not cached and the waiting room is full: refused at once.
        let full = COVER_WAITERS
            .acquire_many(MAX_COVER_WAITERS as u32)
            .await
            .unwrap();
        let refused = other(bot.clone(), token.clone()).await.err().unwrap();
        assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE);
        drop(full);
        // A miss waits for a fetch; a cover finished meanwhile comes from the
        // cache, and the library is not asked.
        let waiting = tokio::spawn(other(bot.clone(), token.clone()));
        while COVER_WAITERS.available_permits() == MAX_COVER_WAITERS {
            tokio::task::yield_now().await;
        }
        bot.covers
            .insert(&(art_key.clone(), "other".into(), 300), &body)
            .await;
        drop((fetches, jobs));
        let response = waiting.await.unwrap().ok().unwrap();
        let served = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(served, body);
        assert_eq!(COVER_WAITERS.available_permits(), MAX_COVER_WAITERS);
        // A miss that finds no fetch within the wait is refused.
        let fetches = COVER_FETCHES
            .acquire_many(MAX_COVER_FETCHES as u32)
            .await
            .unwrap();
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let late = art(
            State(bot.clone()),
            Path((token.clone(), 0, "late".into())),
            Query(ArtQuery { size: 300, v: None }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(late.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() >= COVER_WAIT);
        drop(fetches);
        assert!(server.requests().is_empty());
    }

    /// A 1×1 thumbnail PNG, standing in for what the content worker makes.
    const PNG: &[u8] = b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15\xc4\x89\x00\x00\x00\x0d\x49\x44\x41\x54\x78\x9c\x63\x38\x11\xa0\xf1\x1f\x00\x05\x64\x02\x40\x64\x52\x9c\x3a\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82";

    type Runs = Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

    /// A content worker stand-in that records the images it gets and makes [`PNG`].
    fn recorded(
        runs: &Runs,
    ) -> impl FnOnce(Vec<u8>, u32) -> std::future::Ready<Result<Bytes, ApiError>> + Send + 'static
    {
        let runs = runs.clone();
        move |image, _| {
            runs.lock().unwrap().push(image);
            std::future::ready(Ok(Bytes::from_static(PNG)))
        }
    }

    #[tokio::test]
    async fn byte_identical_cover_images_take_one_worker_job() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::new(|request, socket| {
            use std::io::Write;
            let body: &[u8] = if request.contains("id=mf-other") {
                b"other image"
            } else {
                b"album image"
            };
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(body).unwrap();
        });
        let (_directory, bot, _token) = test_bot(&server.url, 1);
        let library = bot.libraries[0].key().to_owned();
        let runs = Runs::default();
        let cover = |id: String, size: u32| {
            let (bot, convert) = (bot.clone(), recorded(&runs));
            let key: CoverKey = (library.clone(), id.clone(), size);
            async move {
                let libraries = bot.libraries.clone();
                let image = move |fetch| library_image(libraries, 0, id, size, fetch);
                fetch_cover(&bot, key, image, convert).await.ok().unwrap()
            }
        };
        // Twelve songs of one album arrive together, and the library answers
        // each with the album's image.
        let songs: Vec<_> = (0..12)
            .map(|index| tokio::spawn(cover(format!("mf-{index}_0"), 300)))
            .collect();
        for song in songs {
            assert_eq!(song.await.unwrap(), PNG);
        }
        assert_eq!(*runs.lock().unwrap(), [b"album image".to_vec()]);
        assert_eq!(server.requests().len(), 12);
        for index in 0..12 {
            let key = (library.clone(), format!("mf-{index}_0"), 300);
            assert_eq!(bot.covers.get(&key).await.as_deref(), Some(PNG));
        }
        // The album's own cover comes from memory.
        assert_eq!(cover("al-1_0".into(), 300).await, PNG);
        assert_eq!(runs.lock().unwrap().len(), 1);
        // Other bytes, and the same bytes at another size, take their own run.
        assert_eq!(cover("mf-other".into(), 300).await, PNG);
        assert_eq!(cover("mf-0_0".into(), 64).await, PNG);
        assert_eq!(
            *runs.lock().unwrap(),
            [
                b"album image".to_vec(),
                b"other image".to_vec(),
                b"album image".to_vec()
            ]
        );
        assert_eq!(server.requests().len(), 15);
        assert_eq!(COVER_WAITERS.available_permits(), MAX_COVER_WAITERS);
        assert_eq!(COVER_FETCHES.available_permits(), MAX_COVER_FETCHES);
        assert_eq!(COVER_JOBS.available_permits(), MAX_COVER_JOBS);
    }

    #[tokio::test]
    async fn a_cover_waiting_behind_its_image_takes_the_png_and_jobs_have_a_wait_limit() {
        let _exclusive = API_TESTS.lock().await;
        let (_directory, bot, _token) = test_bot("http://unused.invalid", 1);
        let runs = Runs::default();
        let cover = |id: &str, image: &'static [u8]| {
            let (bot, convert) = (bot.clone(), recorded(&runs));
            let key: CoverKey = ("k".into(), id.into(), 300);
            async move {
                let image = move |_fetch| std::future::ready(Ok(image.to_vec()));
                fetch_cover(&bot, key, image, convert).await
            }
        };
        // With both jobs taken, the first song waits for a job and holds its
        // image's turn; the second, with the same image, waits behind it.
        let jobs = COVER_JOBS
            .acquire_many(MAX_COVER_JOBS as u32)
            .await
            .unwrap();
        let shared: ImageKey = (Sha256::digest(b"shared image").into(), 300);
        let first = tokio::spawn(cover("first", b"shared image"));
        let second = tokio::spawn(cover("second", b"shared image"));
        // The map's reference, the first's guard and the second's wait.
        while MAKING
            .lock()
            .unwrap()
            .get(&shared)
            .map_or(0, Arc::strong_count)
            < 3
        {
            tokio::task::yield_now().await;
        }
        drop(jobs);
        assert_eq!(first.await.unwrap().ok().unwrap(), PNG);
        assert_eq!(second.await.unwrap().ok().unwrap(), PNG);
        assert_eq!(*runs.lock().unwrap(), [b"shared image".to_vec()]);
        let key = ("k".into(), "second".into(), 300);
        assert_eq!(bot.covers.get(&key).await.as_deref(), Some(PNG));

        // A miss that finds no job within the wait is refused.
        let jobs = COVER_JOBS
            .acquire_many(MAX_COVER_JOBS as u32)
            .await
            .unwrap();
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let late = cover("late", b"late image").await.err().unwrap();
        assert_eq!(late.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() >= COVER_WAIT);
        drop(jobs);
        assert_eq!(runs.lock().unwrap().len(), 1);
        assert_eq!(COVER_WAITERS.available_permits(), MAX_COVER_WAITERS);
    }

    #[tokio::test]
    async fn blocking_task_diagnostics_are_not_public_responses() {
        static ADMISSION: Semaphore = Semaphore::const_new(1);
        let response = blocking_library_call::<()>(&ADMISSION, LIBRARY_TIMEOUT, || {
            panic!("http://private.example/?apiKey=private-key")
        })
        .await
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response_json(response).await["error"],
            "the music library request failed"
        );
        let private = "http://private.example/?apiKey=private-key";
        for (error, expected) in [
            (
                subsonic::Error::Http(ureq::Error::BadUri(private.into())),
                "the music library request failed",
            ),
            (
                subsonic::Error::Status(500),
                "the music library returned HTTP status 500",
            ),
            (
                subsonic::Error::Api {
                    code: 40,
                    message: private.into(),
                },
                "the music library rejected the request (error 40)",
            ),
            (
                subsonic::Error::Json(serde_json::from_str::<String>("{}").unwrap_err()),
                "the music library returned an invalid reply",
            ),
            (
                subsonic::Error::NotAudio(private.into()),
                "the music library sent a non-audio response",
            ),
            (
                subsonic::Error::NotImage(private.into()),
                "the music library sent a non-image response",
            ),
            (
                subsonic::Error::Undecodable(private.into()),
                "the music library cannot send this song in a playable format",
            ),
        ] {
            let response =
                blocking_library_call::<()>(&ADMISSION, LIBRARY_TIMEOUT, move || Err(error))
                    .await
                    .err()
                    .unwrap()
                    .into_response();
            assert_eq!(response_json(response).await["error"], expected);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn eight_real_workers_retain_admission_after_timeout_and_cancellation() {
        use std::sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        static ADMISSION: Semaphore = Semaphore::const_new(MAX_LIBRARY_CALLS);
        struct Gate(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Gate {
            fn drop(&mut self) {
                let (open, changed) = &*self.0;
                *open.lock().unwrap() = true;
                changed.notify_all();
            }
        }
        let gate = Gate(Arc::new((Mutex::new(false), Condvar::new())));
        let (entered, mut entries) = tokio::sync::mpsc::channel(MAX_LIBRARY_CALLS);
        let mut callers = Vec::new();
        for index in 0..MAX_LIBRARY_CALLS {
            let gate = gate.0.clone();
            let entered = entered.clone();
            callers.push(tokio::spawn(blocking_library_call(
                &ADMISSION,
                if index < 4 {
                    Duration::from_secs(3600)
                } else {
                    Duration::ZERO
                },
                move || {
                    entered.blocking_send(()).unwrap();
                    let (open, changed) = &*gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = changed.wait(open).unwrap();
                    }
                    Ok(())
                },
            )));
        }
        for _ in 0..MAX_LIBRARY_CALLS {
            tokio::time::timeout(Duration::from_secs(2), entries.recv())
                .await
                .unwrap()
                .unwrap();
        }
        for (index, caller) in callers.into_iter().enumerate() {
            if index < 4 {
                caller.abort();
                assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
            } else {
                let response = caller.await.unwrap().err().unwrap().into_response();
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                assert_eq!(
                    response_json(response).await["error"],
                    "the library did not answer in time"
                );
            }
        }
        let started = Arc::new(AtomicUsize::new(0));
        let attempted = started.clone();
        let response = tokio::time::timeout(
            Duration::from_millis(200),
            blocking_library_call(&ADMISSION, LIBRARY_TIMEOUT, move || {
                attempted.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .await
        .unwrap()
        .err()
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            started.load(Ordering::SeqCst),
            0,
            "busy calls must not spawn work"
        );

        drop(gate);
        let returned = tokio::time::timeout(
            Duration::from_secs(2),
            ADMISSION.acquire_many(MAX_LIBRARY_CALLS as u32),
        )
        .await
        .unwrap()
        .unwrap();
        drop(returned);
        assert!(
            blocking_library_call(&ADMISSION, LIBRARY_TIMEOUT, || Ok(()))
                .await
                .is_ok()
        );
    }

    #[test]
    fn historical_library_indices_never_alias_a_replacement() {
        let item: crate::player::HistoryItem = serde_json::from_value(serde_json::json!({
            "id": 1,
            "entry": { "id": 2, "added_by": "Listener", "track": {
                "library": 0, "id": "song", "title": "Historical title",
                "artist": "Historical artist", "album": "Historical album",
                "album_id": "album", "artist_id": "artist", "duration": 60,
                "track": 1, "suffix": "flac", "content_type": "audio/flac", "cover_art": "cover"
            } },
            "source": { "name": "Old library", "url": "https://old.example" },
            "started_at_ms": null, "ended_at_ms": 1000, "played_ms": 0,
            "position_ms": 0, "outcome": "failed", "error": "decode failed"
        }))
        .unwrap();
        let replacement: crate::config::Library = serde_json::from_value(serde_json::json!({
            "name": "New library", "url": "https://new.example"
        }))
        .unwrap();
        let removed = history_view(&item, std::slice::from_ref(&replacement));
        assert_eq!(removed.entry.track.library, None);
        let value = serde_json::to_value(&removed).unwrap();
        assert!(value.get("source").is_none());
        assert!(!value.to_string().contains("https://old.example"));
        let original: crate::config::Library = serde_json::from_value(serde_json::json!({
            "name": "Old library", "url": "https://old.example/"
        }))
        .unwrap();
        let reordered = history_view(&item, &[replacement, original]);
        assert_eq!(reordered.entry.track.library, Some(1));
    }

    #[test]
    fn only_plain_app_files_are_served() {
        assert_eq!(asset_type("index-Bq1x.js"), Some("text/javascript"));
        assert_eq!(asset_type("index-Bq1x.css"), Some("text/css"));
        assert_eq!(asset_type("secret.json"), None);
        assert_eq!(asset_type("noextension"), None);
    }

    #[test]
    fn commands_from_the_app() {
        let add: Command = serde_json::from_str(
            r#"{"command":"add","player":1,"kind":"album","library":0,"id":"a/1","index":null}"#,
        )
        .unwrap();
        assert!(matches!(
            add,
            Command::Add {
                kind: Kind::Album,
                index: None,
                player: 1,
                ..
            }
        ));
        let similar: Command = serde_json::from_str(
            r#"{"command":"add","player":0,"kind":"similar","library":1,"id":"s/1"}"#,
        )
        .unwrap();
        assert!(matches!(
            similar,
            Command::Add {
                kind: Kind::Similar,
                library: 1,
                to: None,
                index: None,
                ..
            }
        ));
        let path: Command = serde_json::from_str(
            r#"{"command":"add","player":0,"kind":"path","library":1,"id":"s1","to":"s 2","index":3}"#,
        )
        .unwrap();
        assert!(matches!(
            &path,
            Command::Add {
                kind: Kind::Path,
                to: Some(to),
                index: Some(3),
                ..
            } if to == "s 2"
        ));
        assert!(
            serde_json::from_str::<Command>(
                r#"{"command":"add","player":0,"kind":"sonic","library":0,"id":"s1"}"#
            )
            .is_err()
        );
        let ducking: Command = serde_json::from_str(
            r#"{"command":"ducking","player":0,"enabled":true,"level":20,"hold_ms":500,"sensitivity":70}"#,
        )
        .unwrap();
        assert!(matches!(
            ducking,
            Command::Ducking {
                ducking: DuckingView {
                    level: 20,
                    hold_ms: 500,
                    ..
                },
                ..
            }
        ));
        assert!(
            serde_json::from_str::<Command>(r#"{"command":"format_disk","player":0}"#).is_err()
        );
    }

    fn fixture_track(id: &str) -> Track {
        serde_json::from_value(serde_json::json!({ "library": 0, "id": id, "title": id })).unwrap()
    }

    #[test]
    fn a_path_from_the_playing_song_continues_it() {
        let library = LibraryIdentity {
            name: "Open".into(),
            url: "https://open.example".into(),
        };
        let other = LibraryIdentity {
            name: "Plain".into(),
            url: "https://plain.example".into(),
        };
        let ids = |tracks: Vec<Track>| tracks.into_iter().map(|track| track.id).collect::<Vec<_>>();
        let path = || vec![fixture_track("s1"), fixture_track("s2")];
        assert_eq!(
            ids(continue_path(
                path(),
                Some((Some(&library), "s1")),
                &library
            )),
            ["s2"]
        );
        for playing in [
            None,
            Some((None, "s1")),
            Some((Some(&other), "s1")),
            Some((Some(&library), "s2")),
        ] {
            assert_eq!(ids(continue_path(path(), playing, &library)), ["s1", "s2"]);
        }
        assert!(continue_path(Vec::new(), Some((Some(&library), "s1")), &library).is_empty());
    }

    #[test]
    fn similar_songs_list_each_recording_once() {
        let track = |id: &str, title: &str, artist: Option<&str>, duration: Option<u32>| {
            serde_json::from_value::<Track>(serde_json::json!({
                "library": 0, "id": id, "title": title, "artist": artist, "duration": duration,
            }))
            .unwrap()
        };
        let found = |track: Track, similarity: Option<f64>| SonicMatch { track, similarity };
        let seed = track("seed", "Paper Boats", Some("Lumen"), Some(206));
        let matches = vec![
            found(seed.clone(), Some(1.0)),
            found(track("exact", "Other", Some("Lumen"), Some(100)), Some(1.0)),
            // The seed on a compilation, scored below 100%.
            found(
                track("best-of", " paper  BOATS", Some("lumen"), Some(208)),
                Some(0.97),
            ),
            found(
                track("hours", "Hours", Some("Nightjar"), Some(250)),
                Some(0.9),
            ),
            found(
                track("hours-live", "Hours (Live)", Some("Nightjar"), Some(280)),
                Some(0.85),
            ),
            found(
                track("hours-again", "Hours", Some("Nightjar"), Some(252)),
                Some(0.8),
            ),
            found(
                track("hours-edit", "Hours", Some("Nightjar"), Some(190)),
                Some(0.75),
            ),
            found(track("unscored", "Salt Air", None, None), None),
            found(track("unscored-copy", "Salt Air", None, Some(200)), None),
            found(
                track("cover", "Hours", Some("Someone Else"), Some(250)),
                Some(0.4),
            ),
        ];
        let kept: Vec<_> = distinct_similar(matches, &seed)
            .into_iter()
            .map(|found| found.track.id)
            .collect();
        assert_eq!(
            kept,
            ["hours", "hours-live", "hours-edit", "unscored", "cover"]
        );
    }

    /// `open` offers sonic similarity; `plain` does not; `failing` offers it
    /// but rejects every call with a message carrying the backend secret.
    fn sonic_fixture(mode: &'static str) -> Server {
        Server::new(move |request, socket| {
            use std::io::Write;
            let body = if request.starts_with("GET /rest/getOpenSubsonicExtensions") {
                if mode == "plain" {
                    r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"transcoding","versions":[1]}]}}"#
                } else {
                    r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"sonicSimilarity","versions":[1]}]}}"#
                }
            } else if mode == "failing" {
                r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"http://dj:secret@private.example/rest/findSonicPath?apiKey=private-key"}}}"#
            } else if request.starts_with("GET /rest/ping") {
                r#"{"subsonic-response":{"status":"ok"}}"#
            } else if request.starts_with("GET /rest/getSong") {
                r#"{"subsonic-response":{"status":"ok","song":{"id":"s1","title":"First"}}}"#
            } else if request.starts_with("GET /rest/getSonicSimilarTracks") {
                // The seed itself and another copy of it sound 100% alike.
                r#"{"subsonic-response":{"status":"ok","sonicMatch":[{"entry":{"id":"s1","title":"First"},"similarity":1.0},{"entry":{"id":"s1-copy","title":"First"},"similarity":1.0},{"entry":{"id":"s2","title":"Second"},"similarity":0.8}]}}"#
            } else if request.starts_with("GET /rest/findSonicPath") {
                r#"{"subsonic-response":{"status":"ok","sonicMatch":[{"entry":{"id":"s1","title":"First"},"similarity":1.0},{"entry":{"id":"s2","title":"Second"},"similarity":0.5}]}}"#
            } else {
                r#"{"subsonic-response":{"status":"failed","error":{"code":0}}}"#
            };
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(body.as_bytes()).unwrap();
        })
    }

    fn queue_ids(bot: &Bot) -> Vec<String> {
        bot.players[0]
            .state()
            .queue
            .iter()
            .map(|entry| entry.track.id.clone())
            .collect()
    }

    #[test]
    fn snapshot_reports_only_already_known_sonic_support() {
        let server = sonic_fixture("open");
        let (_directory, bot, token) = test_bot(&server.url, 2);
        let you = owner(&bot, &token).ok().unwrap();
        assert_eq!(snapshot(&bot, &you).sonic, [false, false]);
        assert!(server.requests().is_empty(), "snapshots never ask");
        bot.libraries[1].ping().unwrap();
        let asked = server.requests().len();
        // Library 0 would answer the same, but nobody has asked it yet.
        assert_eq!(snapshot(&bot, &you).sonic, [false, true]);
        assert_eq!(server.requests().len(), asked);
    }

    #[tokio::test]
    async fn a_song_page_lists_the_albums_with_the_same_song() {
        let _exclusive = API_TESTS.lock().await;
        for search_fails in [false, true] {
            let server = Server::new(move |request, socket| {
                use std::io::Write;
                let body = if request.starts_with("GET /rest/getSong") {
                    r#"{"subsonic-response":{"status":"ok","song":{"id":"s1","title":"Tide","artist":"Waves","album":"Sea","albumId":"a1","year":2021,"genre":"Ambient","bitRate":320,"discNumber":2,"playCount":7}}}"#
                } else if search_fails {
                    r#"{"subsonic-response":{"status":"failed","error":{"code":0,"message":"http://dj:secret@private.example/rest/search3"}}}"#
                } else {
                    // Same song on two other albums (one twice), a cover by
                    // someone else, a near title and a song without album.
                    r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[
                        {"id":"s1","title":"Tide","artist":"Waves","albumId":"a1"},
                        {"id":"s2","title":" tide ","artist":"WAVES","album":"Best of","albumId":"a2"},
                        {"id":"s3","title":"Tide","artist":"Waves","album":"Best of","albumId":"a2"},
                        {"id":"s4","title":"Tide","artist":"Someone","albumId":"a3"},
                        {"id":"s5","title":"Tide (Live)","artist":"Waves","albumId":"a4"},
                        {"id":"s6","title":"Tide","artist":"Waves"},
                        {"id":"s7","title":"Tide","artist":"Waves","album":"Live","albumId":"a5"}]}}}"#
                };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(body.as_bytes()).unwrap();
            });
            let (_directory, bot, token) = test_bot(&server.url, 1);
            let view = song(
                State(bot),
                Path(token),
                Query(ItemQuery {
                    library: 0,
                    id: "s1".into(),
                }),
            )
            .await
            .ok()
            .unwrap()
            .0;
            assert_eq!(view.song.title, "Tide");
            assert_eq!(
                view.details,
                SongDetails {
                    year: Some(2021),
                    genre: Some("Ambient".into()),
                    bit_rate: Some(320),
                    disc_number: Some(2),
                    play_count: Some(7),
                }
            );
            let albums: Vec<_> = view
                .appearances
                .iter()
                .map(|track| track.album_id.as_deref().unwrap())
                .collect();
            if search_fails {
                assert!(albums.is_empty());
                let error = view.appearances_error.unwrap();
                assert!(!error.contains("secret"), "{error}");
            } else {
                assert_eq!(albums, ["a1", "a2", "a5"]);
                assert_eq!(
                    view.appearances[0].id, "s1",
                    "the song's own album comes first"
                );
                assert!(view.appearances_error.is_none());
            }
        }
    }

    #[tokio::test]
    async fn sonic_lists_and_adds_follow_the_library_reply() {
        let _exclusive = API_TESTS.lock().await;
        let server = sonic_fixture("open");
        let (_directory, mut bot, token) = test_bot(&server.url, 2);
        let unique = Arc::get_mut(&mut bot).unwrap();
        unique.players.push(Arc::new(crate::player::Player::new(
            "Queue".into(),
            50,
            Ducking::default(),
            unique.changes.clone(),
        )));

        let found = similar(
            State(bot.clone()),
            Path(token.clone()),
            Query(ItemQuery {
                library: 1,
                id: "s1".into(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!((found.song.library, found.song.id.as_str()), (1, "s1"));
        let found = serde_json::to_value(&found).unwrap();
        // The list leaves out the song itself and its 100% copy.
        let listed: Vec<_> = found["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|found| found["track"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(listed, ["s2"]);
        assert_eq!(found["matches"][0]["track"]["library"], 1);
        assert_eq!(found["matches"][0]["similarity"], 0.8);

        let path = sonic_path(
            State(bot.clone()),
            Path(token.clone()),
            Query(PathQuery {
                library: 1,
                from: "s1".into(),
                to: "s2".into(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!(
            path.matches
                .iter()
                .map(|found| (found.track.id.as_str(), found.similarity))
                .collect::<Vec<_>>(),
            [("s1", Some(1.0)), ("s2", Some(0.5))]
        );
        let missing = sonic_path(
            State(bot.clone()),
            Path(token.clone()),
            Query(PathQuery {
                library: 2,
                from: "s1".into(),
                to: "s2".into(),
            }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            (missing.0, missing.1.as_str()),
            (StatusCode::BAD_GATEWAY, "no such library")
        );

        let add = |kind, to: Option<&str>| Command::Add {
            player: 0,
            kind,
            library: 1,
            id: "s1".into(),
            to: to.map(str::to_owned),
            page: None,
            index: None,
        };
        let asked = server.requests().len();
        let rejected = command(
            State(bot.clone()),
            Path(token.clone()),
            Json(add(Kind::Path, None)),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(rejected.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            server.requests().len(),
            asked,
            "rejected before the library"
        );
        command(
            State(bot.clone()),
            Path(token.clone()),
            Json(add(Kind::Similar, None)),
        )
        .await
        .ok()
        .unwrap();
        // Adding them all skips the seed and its identical copy.
        assert_eq!(queue_ids(&bot), ["s2"]);
        // Nothing plays, so the path is queued from its start.
        command(
            State(bot.clone()),
            Path(token.clone()),
            Json(add(Kind::Path, Some("s2"))),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(queue_ids(&bot), ["s2", "s1", "s2"]);
    }

    #[tokio::test]
    async fn sonic_endpoints_need_the_extension_redact_errors_and_reject_overload() {
        let _exclusive = API_TESTS.lock().await;
        async fn both(bot: &Arc<Bot>, token: &str) -> [ApiError; 2] {
            let similar = similar(
                State(bot.clone()),
                Path(token.to_owned()),
                Query(ItemQuery {
                    library: 0,
                    id: "s1".into(),
                }),
            );
            let path = sonic_path(
                State(bot.clone()),
                Path(token.to_owned()),
                Query(PathQuery {
                    library: 0,
                    from: "s1".into(),
                    to: "s2".into(),
                }),
            );
            let limit = Duration::from_secs(5);
            [
                tokio::time::timeout(limit, similar)
                    .await
                    .unwrap()
                    .err()
                    .unwrap(),
                tokio::time::timeout(limit, path)
                    .await
                    .unwrap()
                    .err()
                    .unwrap(),
            ]
        }

        let plain = sonic_fixture("plain");
        let (_directory, bot, token) = test_bot(&plain.url, 1);
        for error in both(&bot, &token).await {
            assert_eq!(
                (error.0, error.1.as_str()),
                (
                    StatusCode::BAD_GATEWAY,
                    "this music library does not support sonic similarity"
                )
            );
        }
        let requests = plain.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /rest/getOpenSubsonicExtensions"));

        let failing = sonic_fixture("failing");
        let (_directory, bot, token) = test_bot(&failing.url, 1);
        for error in both(&bot, &token).await {
            assert_eq!(
                (error.0, error.1.as_str()),
                (
                    StatusCode::BAD_GATEWAY,
                    "the music library rejected the request (error 40)"
                )
            );
        }
        assert!(
            failing
                .requests()
                .iter()
                .any(|request| request.starts_with("GET /rest/findSonicPath"))
        );

        let occupied = LIBRARY_CALLS
            .acquire_many(MAX_LIBRARY_CALLS as u32)
            .await
            .unwrap();
        let asked = failing.requests().len();
        for error in both(&bot, &token).await {
            assert_eq!(
                (error.0, error.1.as_str()),
                (StatusCode::SERVICE_UNAVAILABLE, "the music library is busy")
            );
        }
        assert_eq!(failing.requests().len(), asked);
        drop(occupied);
    }

    #[tokio::test]
    async fn lyrics_answer_the_lines_or_null() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::new(|request, socket| {
            use std::io::Write;
            let body = if request.starts_with("GET /rest/getOpenSubsonicExtensions") {
                r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"songLyrics","versions":[1]}]}}"#
            } else if request.contains("&id=s1 ") {
                r#"{"subsonic-response":{"status":"ok","lyricsList":{"structuredLyrics":[{"lang":"eng","synced":true,"line":[{"start":1500,"value":"Hello"}]}]}}}"#
            } else {
                r#"{"subsonic-response":{"status":"ok","lyricsList":{"structuredLyrics":[]}}}"#
            };
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let (_directory, bot, token) = test_bot(&server.url, 1);
        let ask = |id: &str| {
            lyrics(
                State(bot.clone()),
                Path(token.clone()),
                Query(ItemQuery {
                    library: 0,
                    id: id.into(),
                }),
            )
        };
        let found = ask("s1").await.ok().unwrap();
        assert_eq!(
            serde_json::to_value(&found.0).unwrap(),
            serde_json::json!({"lyrics": {"synced": true, "lang": "eng",
                "lines": [{"start_ms": 1500, "text": "Hello"}]}})
        );
        let none = ask("s2").await.ok().unwrap();
        assert_eq!(
            serde_json::to_value(&none.0).unwrap(),
            serde_json::json!({"lyrics": null})
        );
        let missing = lyrics(
            State(bot.clone()),
            Path(Uuid::nil().to_string()),
            Query(ItemQuery {
                library: 0,
                id: "s1".into(),
            }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn suggestions_ask_for_few_matches_and_reject_overload() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::new(|request, socket| {
            use std::io::Write;
            let body = if request.contains("query=broken") {
                serde_json::json!({"subsonic-response": {"status": "failed", "error": {"code": 0}}})
            } else {
                // More of each kind than asked for, as a server may send.
                let entries = |prefix: &str, count: usize| {
                    (0..count)
                        .map(|n| {
                            serde_json::json!({"id": format!("{prefix}{n}"),
                            "name": format!("{prefix} {n}"), "title": format!("{prefix} {n}"),
                            "artist": "Waves", "coverArt": format!("c-{prefix}{n}")})
                        })
                        .collect::<Vec<_>>()
                };
                // The second song is a copy of the first on another album,
                // spelled with other case and spacing.
                let mut songs = entries("s", 8);
                songs.insert(
                    1,
                    serde_json::json!({"id": "s0-copy", "title": "S  0", "artist": "waves",
                        "coverArt": "c-s0-copy"}),
                );
                serde_json::json!({"subsonic-response": {"status": "ok", "searchResult3": {
                    "artist": entries("ar", 5), "album": entries("al", 5), "song": songs}}})
            };
            let body = serde_json::to_vec(&body).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(&body).unwrap();
        });
        let (_directory, bot, token) = test_bot(&server.url, 2);
        let ask = |q: &str| {
            suggest(
                State(bot.clone()),
                Path(token.clone()),
                Query(SearchQuery { q: q.into() }),
            )
        };

        for short in ["", "w", "  w  ", "ü"] {
            assert!(ask(short).await.ok().unwrap().items.is_empty());
        }
        assert!(server.requests().is_empty());

        let items = ask(" wa ").await.ok().unwrap().0.items;
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert!(request.starts_with("GET /rest/search3?"));
            for parameter in [
                "&query=wa&",
                "&songCount=5&",
                "&albumCount=3&",
                "&artistCount=3",
            ] {
                assert!(request.contains(parameter), "{parameter} in {request}");
            }
        }
        // Both libraries answer alike: each artist and album shows once, from the
        // first library, then songs until the bound.
        assert_eq!(items.len(), MAX_SUGGESTIONS);
        let order: Vec<_> = items
            .iter()
            .map(|item| (item.library, item.id.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                (0, "ar0"),
                (0, "ar1"),
                (0, "ar2"),
                (0, "al0"),
                (0, "al1"),
                (0, "al2"),
                (0, "s0"),
                (0, "s1"),
                (0, "s2"),
                (0, "s3"),
            ]
        );
        assert_eq!(
            serde_json::to_value(&items[0]).unwrap(),
            serde_json::json!({"library": 0, "kind": "artist", "id": "ar0", "title": "ar 0",
                "subtitle": null, "cover_art": "c-ar0"})
        );
        assert_eq!(
            serde_json::to_value(&items[3]).unwrap(),
            serde_json::json!({"library": 0, "kind": "album", "id": "al0", "title": "al 0",
                "subtitle": "Waves", "cover_art": "c-al0"})
        );

        // One library: the songs follow, cut at the bound.
        let (_single, one, one_token) = test_bot(&server.url, 1);
        let items = suggest(
            State(one),
            Path(one_token),
            Query(SearchQuery { q: "wa".into() }),
        )
        .await
        .ok()
        .unwrap()
        .0
        .items;
        let kinds: Vec<_> = items.iter().map(|item| &item.kind).collect();
        assert_eq!(
            kinds,
            [
                [&SuggestionKind::Artist; 3].as_slice(),
                &[&SuggestionKind::Album; 3],
                &[&SuggestionKind::Song; 4],
            ]
            .concat()
        );
        assert_eq!(
            serde_json::to_value(&items[6]).unwrap(),
            serde_json::json!({"library": 0, "kind": "song", "id": "s0", "title": "s 0",
                "subtitle": "Waves", "cover_art": "c-s0"})
        );

        // Failing libraries add nothing.
        assert!(ask("broken").await.ok().unwrap().items.is_empty());

        let occupied = LIBRARY_CALLS
            .acquire_many(MAX_LIBRARY_CALLS as u32)
            .await
            .unwrap();
        let asked = server.requests().len();
        let refused = tokio::time::timeout(Duration::from_millis(200), ask("wa"))
            .await
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(
            (refused.0, refused.1.as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, "the music library is busy")
        );
        assert_eq!(server.requests().len(), asked);
        drop(occupied);
    }

    /// The decoded query parameters of a fixture request.
    fn request_parameters(request: &str) -> std::collections::HashMap<String, String> {
        let target: axum::http::Uri = request.split(' ').nth(1).unwrap().parse().unwrap();
        Query::try_from_uri(&target).unwrap().0
    }

    fn fixture_json(socket: &mut std::net::TcpStream, body: &serde_json::Value) {
        use std::io::Write;
        let body = serde_json::to_vec(body).unwrap();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        socket.write_all(&body).unwrap();
    }

    #[tokio::test]
    async fn genres_come_by_name_with_lenient_counts_and_bounded() {
        let _exclusive = API_TESTS.lock().await;
        let long = "x".repeat(MAX_GENRE_NAME + 1);
        let server = Server::new(move |request, socket| {
            assert!(request.starts_with("GET /rest/getGenres?"), "{request}");
            fixture_json(
                socket,
                &serde_json::json!({"subsonic-response": {"status": "ok", "genres": {"genre": [
                    {"value": "rock", "songCount": 12, "albumCount": 2},
                    {"value": "Ambient", "songCount": "7", "albumCount": " 1 "},
                    {"value": "Jazz; Fusion", "songCount": -3, "albumCount": 2.5},
                    {"value": 1990, "songCount": null},
                    {"value": "Ambient", "songCount": 7, "albumCount": 1},
                    {"value": "", "songCount": 1},
                    {"value": long, "songCount": 1},
                    {"value": true, "songCount": 1},
                    {"songCount": 1},
                    {"value": "Électro", "songCount": 99_999_999_999u64, "albumCount": "many"},
                ]}}}),
            );
        });
        let (_directory, bot, token) = test_bot(&server.url, 1);
        let list = genres(
            State(bot.clone()),
            Path(token.clone()),
            Query(LibraryQuery { library: 0 }),
        )
        .await
        .ok()
        .unwrap()
        .0;
        assert_eq!(
            serde_json::to_value(&list).unwrap(),
            serde_json::json!({"genres": [
                {"name": "1990", "albums": null, "songs": null},
                {"name": "Ambient", "albums": 1, "songs": 7},
                {"name": "Jazz; Fusion", "albums": null, "songs": null},
                {"name": "rock", "albums": 2, "songs": 12},
                {"name": "Électro", "albums": null, "songs": u32::MAX},
            ], "truncated": false})
        );

        let many = Server::new(|_, socket| {
            let genre: Vec<_> = (0..subsonic::MAX_GENRES + 100)
                .rev()
                .map(|n| serde_json::json!({"value": format!("g{n:05}"), "songCount": n}))
                .collect();
            fixture_json(
                socket,
                &serde_json::json!({"subsonic-response": {"status": "ok", "genres": {"genre": genre}}}),
            );
        });
        let (_many_directory, bot, token) = test_bot(&many.url, 1);
        let list = genres(State(bot), Path(token), Query(LibraryQuery { library: 0 }))
            .await
            .ok()
            .unwrap()
            .0;
        assert!(list.truncated);
        assert_eq!(list.genres.len(), subsonic::MAX_GENRES);
        assert_eq!(list.genres[0].name, "g00000");
        assert_eq!(list.genres.last().unwrap().name, "g01999");
    }

    #[tokio::test]
    async fn genre_pages_ask_by_name_and_page_and_keep_odd_names() {
        let _exclusive = API_TESTS.lock().await;
        // Full pages from offset 0, a short page after it, and more songs
        // than asked for, as a server may send.
        let server = Server::new(|request, socket| {
            let parameters = request_parameters(request);
            let genre = &parameters["genre"];
            let offset: usize = parameters["offset"].parse().unwrap();
            let body = if request.starts_with("GET /rest/getAlbumList2?") {
                assert_eq!(parameters["type"], "byGenre");
                let size: usize = parameters["size"].parse().unwrap();
                let count = if offset == 0 { size } else { 3 };
                let album: Vec<_> = (offset..offset + count)
                    .map(|n| serde_json::json!({"id": format!("al{n}"), "name": format!("{genre} #{n}")}))
                    .collect();
                serde_json::json!({"subsonic-response": {"status": "ok", "albumList2": {"album": album}}})
            } else {
                assert!(
                    request.starts_with("GET /rest/getSongsByGenre?"),
                    "{request}"
                );
                let count: usize = parameters["count"].parse().unwrap();
                let song: Vec<_> = (offset..offset + count + 10)
                    .map(|n| serde_json::json!({"id": format!("s{n}"), "title": genre}))
                    .collect();
                serde_json::json!({"subsonic-response": {"status": "ok", "songsByGenre": {"song": song}}})
            };
            fixture_json(socket, &body);
        });
        let (_directory, mut bot, token) = test_bot(&server.url, 1);
        let unique = Arc::get_mut(&mut bot).unwrap();
        unique.players.push(Arc::new(crate::player::Player::new(
            "Queue".into(),
            50,
            Ducking::default(),
            unique.changes.clone(),
        )));
        let query = |name: &str, page| {
            Query(GenreQuery {
                library: 0,
                name: name.to_owned(),
                page,
            })
        };
        let longest = "é".repeat(MAX_GENRE_NAME);
        for name in [
            "Rock; Pop",
            "R&B",
            "AC/DC Covers",
            "Drum + Bass",
            "a=b?c#d%20",
            "Música Popular Brasileira 日本",
            longest.as_str(),
        ] {
            let first = genre_albums(State(bot.clone()), Path(token.clone()), query(name, 0))
                .await
                .ok()
                .unwrap()
                .0;
            assert_eq!(first.albums.len(), PAGE_SIZE as usize);
            assert_eq!(first.albums[0].name, format!("{name} #0"));
            assert!(first.more);
            let second = genre_albums(State(bot.clone()), Path(token.clone()), query(name, 1))
                .await
                .ok()
                .unwrap()
                .0;
            assert_eq!(second.albums[0].name, format!("{name} #48"));
            assert!(!second.more);
            let songs = genre_songs(State(bot.clone()), Path(token.clone()), query(name, 2))
                .await
                .ok()
                .unwrap()
                .0;
            assert_eq!(songs.songs.len(), GENRE_SONGS_PAGE as usize);
            assert_eq!(songs.songs[0].id, "s100");
            assert_eq!(songs.songs[0].title, name);
            assert!(songs.more);
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 21);
        assert!(requests[0].contains("&size=48&offset=0"), "{}", requests[0]);
        assert!(
            requests[1].contains("&size=48&offset=48"),
            "{}",
            requests[1]
        );
        assert!(
            requests[2].contains("&count=50&offset=100"),
            "{}",
            requests[2]
        );
        assert!(
            requests[0].contains("&genre=Rock%3B%20Pop&"),
            "{}",
            requests[0]
        );

        // Adding a genre queues one page of its songs.
        let add: Command = serde_json::from_str(
            r#"{"command":"add","player":0,"kind":"genre","library":0,"id":"R&B","page":1}"#,
        )
        .unwrap();
        command(State(bot.clone()), Path(token.clone()), Json(add))
            .await
            .ok()
            .unwrap();
        let queued = queue_ids(&bot);
        assert_eq!(queued.len(), GENRE_SONGS_PAGE as usize);
        assert_eq!((queued[0].as_str(), queued[49].as_str()), ("s50", "s99"));
        assert!(server.requests()[21].contains("&genre=R%26B&count=50&offset=50"));

        // Names the bot never asks for are refused before the library.
        let too_long = "é".repeat(MAX_GENRE_NAME + 1);
        let asked = server.requests().len();
        for name in ["", too_long.as_str()] {
            let refused = genre_albums(State(bot.clone()), Path(token.clone()), query(name, 0))
                .await
                .err()
                .unwrap();
            assert_eq!(refused.0, StatusCode::BAD_REQUEST);
            let refused = genre_songs(State(bot.clone()), Path(token.clone()), query(name, 0))
                .await
                .err()
                .unwrap();
            assert_eq!(refused.0, StatusCode::BAD_REQUEST);
            let add = Command::Add {
                player: 0,
                kind: Kind::Genre,
                library: 0,
                id: name.to_owned(),
                to: None,
                page: None,
                index: None,
            };
            let refused = command(State(bot.clone()), Path(token.clone()), Json(add))
                .await
                .err()
                .unwrap();
            assert_eq!(refused.0, StatusCode::BAD_REQUEST);
        }
        assert_eq!(server.requests().len(), asked);

        let occupied = LIBRARY_CALLS
            .acquire_many(MAX_LIBRARY_CALLS as u32)
            .await
            .unwrap();
        let busy = [
            genres(
                State(bot.clone()),
                Path(token.clone()),
                Query(LibraryQuery { library: 0 }),
            )
            .await
            .err()
            .unwrap(),
            genre_albums(State(bot.clone()), Path(token.clone()), query("R&B", 0))
                .await
                .err()
                .unwrap(),
            genre_songs(State(bot.clone()), Path(token.clone()), query("R&B", 0))
                .await
                .err()
                .unwrap(),
        ];
        for refused in busy {
            assert_eq!(
                (refused.0, refused.1.as_str()),
                (StatusCode::SERVICE_UNAVAILABLE, "the music library is busy")
            );
        }
        assert_eq!(server.requests().len(), asked);
        drop(occupied);
    }

    #[test]
    fn import_covers_have_a_namespace_no_library_key_can_take() {
        assert!(!crate::library_keys::is_key(IMPORT_NAMESPACE));
        let (_directory, bot, _token) = test_bot("http://unused.invalid", 2);
        assert!(
            bot.libraries
                .iter()
                .all(|library| library.key() != IMPORT_NAMESPACE)
        );
    }

    #[test]
    fn state_has_no_import_without_the_section() {
        let (_directory, bot, token) = test_bot("http://unused.invalid", 1);
        let viewer = owner(&bot, &token).ok().unwrap();
        let state = serde_json::to_value(snapshot(&bot, &viewer)).unwrap();
        assert_eq!(state["import"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn import_endpoints_need_the_section() {
        let (_directory, bot, token) = test_bot("http://unused.invalid", 1);
        let error = import_requests(State(bot), Path(token))
            .await
            .err()
            .unwrap();
        assert_eq!(
            (error.0, error.1.as_str()),
            (
                StatusCode::NOT_FOUND,
                "Adding music is not available right now."
            )
        );
    }

    #[tokio::test]
    async fn import_endpoints_pass_on_checked_replies_and_fixed_errors() {
        use std::{
            io::Write,
            sync::{
                Mutex,
                atomic::{AtomicBool, Ordering as AtomicOrdering},
            },
        };
        let _exclusive = API_TESTS.lock().await;
        const ALBUM: &str = "0123456789abcdef";
        let album = serde_json::json!({
            "id": ALBUM, "title": "The  Album", "artist": "the artist", "year": 2001,
            "track_count": 2, "release_type": "album", "has_cover": true,
        });
        let import = serde_json::json!({
            "id": "00000000000000aa", "album_id": ALBUM, "status": "queued",
            "title": "The Album", "artist": "The Artist", "year": 2001,
            "release_type": "album", "has_cover": true, "tracks_total": 2,
            "tracks_done": 0, "tracks_failed": 0, "requested_at": 1,
            "handed_over_at": null, "mine": true,
        });
        let budget = serde_json::json!({"limit": 100, "used": 2, "next_free_at": null});
        let limited = Arc::new(AtomicBool::new(false));
        let service = Server::new({
            let (album, import, budget, limited) = (
                album.clone(),
                import.clone(),
                budget.clone(),
                limited.clone(),
            );
            move |request, socket| {
                let target = request.split_whitespace().nth(1).unwrap();
                let path = target.split('?').next().unwrap();
                let (status, body) = match (request.split(' ').next().unwrap(), path) {
                    (_, _) if limited.load(AtomicOrdering::SeqCst) => (
                        "429 Too Many Requests",
                        serde_json::json!({"error": "rate_limited", "retry_after": 2}),
                    ),
                    ("GET", "/api/integration/v1/search") => {
                        ("200 OK", serde_json::json!({"albums": [album]}))
                    }
                    ("GET", "/api/integration/v1/albums/0123456789abcdef") => {
                        let mut details = album.clone();
                        details["tracks"] = serde_json::json!([
                            {"position": 1, "title": "One", "artist": "The Artist", "duration": 61},
                            {"position": 2, "title": "Two", "artist": "The Artist", "duration": null},
                        ]);
                        ("200 OK", details)
                    }
                    ("GET", "/api/integration/v1/imports") => (
                        "200 OK",
                        serde_json::json!({"imports": [import],
                            "budget": {"requester": budget, "total": budget}}),
                    ),
                    ("POST", "/api/integration/v1/imports") => {
                        ("202 Accepted", serde_json::json!({"import": import}))
                    }
                    _ => ("404 Not Found", serde_json::json!({"error": "not_found"})),
                };
                let body = serde_json::to_vec(&body).unwrap();
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let searched = Arc::new(Mutex::new(Vec::new()));
        let library = Server::new({
            let searched = searched.clone();
            move |request, socket| {
                searched.lock().unwrap().push(request.to_owned());
                let body = serde_json::to_vec(&serde_json::json!({"subsonic-response": {
                "status": "ok", "searchResult3": {"album": [
                    {"id": "other", "name": "The Album (Live)", "artist": "The Artist"},
                    {"id": "held", "name": "the album", "artist": "The  Artist"},
                ]}}}))
                .unwrap();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let (directory, mut bot, token) = test_bot(&library.url, 1);
        std::fs::write(directory.path().join("import-key"), "import-secret\n").unwrap();
        let config = crate::config::Import {
            url: format!("{}/", service.url),
            key_file: directory.path().join("import-key"),
            library: "Library 0".into(),
        };
        let adding = import::Adding::open(&config, &bot.config.libraries).unwrap();
        Arc::get_mut(&mut bot).unwrap().adding = Some(Arc::new(adding));
        let viewer = bot.tokens.owner(&token.parse().unwrap()).unwrap();
        let requester = bot.pseudonyms.of(&viewer);
        let state = serde_json::to_value(snapshot(&bot, &viewer)).unwrap();
        assert_eq!(
            state["import"],
            serde_json::json!({"library": 0, "revision": 0})
        );
        fn value<T: Serialize>(json: &T) -> serde_json::Value {
            serde_json::to_value(json).unwrap()
        }

        let found = import_search(
            State(bot.clone()),
            Path(token.clone()),
            Query(SearchQuery {
                q: "  the album ".into(),
            }),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(value(&found.0), serde_json::json!({"albums": [album]}));
        let asked = service.requests().pop().unwrap();
        assert!(
            asked.starts_with(&format!(
                "GET /api/integration/v1/search?requester={requester}&q=the%20album&limit=25 "
            )),
            "{asked}"
        );
        assert!(
            asked
                .to_ascii_lowercase()
                .contains("authorization: bearer import-secret\r\n")
        );

        let page = import_album(
            State(bot.clone()),
            Path(token.clone()),
            Query(ImportIdQuery { id: ALBUM.into() }),
        )
        .await
        .ok()
        .unwrap();
        let page = value(&page.0);
        assert_eq!(page["album"]["tracks"][0]["title"], "One");
        assert_eq!(page["album"]["title"], "The  Album");
        assert_eq!(
            page["in_library"],
            serde_json::json!({"library": 0, "id": "held"})
        );
        assert!(searched.lock().unwrap()[0].contains("query=the%20artist%20The%20%20Album"));

        let asked = service.requests().len();
        for id in [
            "",
            "0123456789ABCDEF",
            "../imports/0000",
            "0123456789abcdef0",
        ] {
            let error = import_album(
                State(bot.clone()),
                Path(token.clone()),
                Query(ImportIdQuery { id: id.into() }),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(
                (error.0, error.1.as_str()),
                (StatusCode::NOT_FOUND, "This album cannot be added.")
            );
            let error = import_art(
                State(bot.clone()),
                Path((token.clone(), id.into())),
                Query(ImportArtQuery { size: 64 }),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.0, StatusCode::NOT_FOUND);
            let error = import_request(
                State(bot.clone()),
                Path(token.clone()),
                Json(NewImportRequest {
                    album_id: id.into(),
                }),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.0, StatusCode::NOT_FOUND);
        }
        assert_eq!(
            service.requests().len(),
            asked,
            "invalid ids reached the service"
        );

        let list = import_requests(State(bot.clone()), Path(token.clone()))
            .await
            .ok()
            .unwrap();
        assert_eq!(
            value(&list.0),
            serde_json::json!({"requests": [import], "budget": {"mine": budget, "total": budget}})
        );

        let added = import_request(
            State(bot.clone()),
            Path(token.clone()),
            Json(NewImportRequest {
                album_id: ALBUM.into(),
            }),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(
            value(&added.0),
            serde_json::json!({"request": import, "existing": false})
        );
        let posted = service.requests().pop().unwrap();
        assert!(
            posted.starts_with("POST /api/integration/v1/imports "),
            "{posted}"
        );

        limited.store(true, AtomicOrdering::SeqCst);
        let error = import_search(
            State(bot.clone()),
            Path(token.clone()),
            Query(SearchQuery { q: "x".into() }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            (error.0, error.1.as_str()),
            (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many searches at once. Try again in a few seconds."
            )
        );

        let occupied = import::IMPORT_CALLS
            .acquire_many(import::MAX_IMPORT_CALLS as u32)
            .await
            .unwrap();
        let asked = service.requests().len();
        let error = import_requests(State(bot.clone()), Path(token.clone()))
            .await
            .err()
            .unwrap();
        assert_eq!(
            (error.0, error.1.as_str()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many requests at once. Try again in a moment."
            )
        );
        assert_eq!(service.requests().len(), asked);
        drop(occupied);

        let missing = Uuid::nil().to_string();
        let error = import_requests(State(bot.clone()), Path(missing))
            .await
            .err()
            .unwrap();
        assert_eq!(error.0, StatusCode::NOT_FOUND);
    }

    fn dj_bot(url: &str, libraries: usize) -> (tempfile::TempDir, Arc<Bot>, String) {
        let (directory, mut bot, token) = test_bot(url, libraries);
        let unique = Arc::get_mut(&mut bot).unwrap();
        unique.players.push(Arc::new(crate::player::Player::new(
            "DJ queue".into(),
            50,
            Ducking::default(),
            unique.changes.clone(),
        )));
        (directory, bot, token)
    }

    #[tokio::test]
    async fn dj_controls_are_json_posts_with_checked_ranges() {
        let _exclusive = API_TESTS.lock().await;
        let server = Server::new(|_, socket| {
            use std::io::Write;
            let body = r#"{"subsonic-response":{"status":"ok","song":{"id":"s1","title":"Library song","duration":200,"bpm":126}}}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let (_directory, bot, token) = dj_bot(&server.url, 1);
        let send = |value: serde_json::Value| {
            let (bot, token) = (bot.clone(), token.clone());
            async move {
                let command: DjCommand = serde_json::from_value(value).unwrap();
                dj(State(bot), Path(token), Json(command)).await
            }
        };
        let status = |result: Result<StatusCode, ApiError>| match result {
            Ok(status) => status,
            Err(error) => error.0,
        };
        let view = || {
            serde_json::to_value(snapshot(&bot, &owner(&bot, &token).ok().unwrap())).unwrap()
                ["players"][0]["dj"]
                .clone()
        };
        let volume = serde_json::json!({"player":0,"control":"volume","deck":0,"volume":50});
        assert_eq!(status(send(volume.clone()).await), StatusCode::CONFLICT);
        assert_eq!(
            status(send(serde_json::json!({"player":3,"control":"mode","on":true})).await),
            StatusCode::BAD_REQUEST
        );
        assert!(
            serde_json::from_value::<DjCommand>(
                serde_json::json!({"player":0,"control":"scratch","deck":0})
            )
            .is_err()
        );
        bot.players[0].enqueue(
            vec![
                fixture_track("q1"),
                fixture_track("q2"),
                fixture_track("q3"),
            ],
            "ada",
            None,
        );
        let on = serde_json::json!({"player":0,"control":"mode","on":true});
        assert_eq!(status(send(on).await), StatusCode::NO_CONTENT);
        assert_eq!(view()["decks"][0]["entry"]["track"]["id"], "q1");
        assert_eq!(view()["decks"][1]["entry"]["track"]["id"], "q2");
        for refused in [
            serde_json::json!({"player":0,"control":"volume","deck":0,"volume":101}),
            serde_json::json!({"player":0,"control":"eq","deck":0,"band":"low","db":6.5}),
            serde_json::json!({"player":0,"control":"eq","deck":0,"band":"high","db":-27}),
            serde_json::json!({"player":0,"control":"filter","deck":1,"value":1.01}),
            serde_json::json!({"player":0,"control":"tempo","deck":0,"tempo":16.5}),
            serde_json::json!({"player":0,"control":"tempo_range","deck":0,"range":10}),
            serde_json::json!({"player":0,"control":"crossfader","position":-1.5}),
            serde_json::json!({"player":0,"control":"transition","seconds":0}),
            serde_json::json!({"player":0,"control":"transition","seconds":61}),
            serde_json::json!({"player":0,"control":"seek","deck":0,"position_ms":10_800_001}),
            serde_json::json!({"player":0,"control":"play","deck":2,"playing":true}),
            serde_json::json!({"player":0,"control":"load","deck":0,"library":0,"id":"s1","entry":3}),
            serde_json::json!({"player":0,"control":"load","deck":0}),
            serde_json::json!({"player":0,"control":"load","deck":0,"library":0,"id":""}),
        ] {
            assert_eq!(
                status(send(refused.clone()).await),
                StatusCode::BAD_REQUEST,
                "{refused}"
            );
        }
        // A library song replaces deck B's unheard queue song, which returns
        // to the front of the queue.
        let load = serde_json::json!({"player":0,"control":"load","deck":1,"library":0,"id":"s1"});
        assert_eq!(status(send(load).await), StatusCode::NO_CONTENT);
        let deck = view()["decks"][1].clone();
        assert_eq!(deck["entry"]["track"]["title"], "Library song");
        assert_eq!(deck["entry"]["added_by"], "Listener");
        assert_eq!(deck["bpm"], 126.0);
        assert_eq!(deck["bpm_source"], "library");
        assert_eq!(queue_ids(&bot), ["q2", "q3"]);
        let q3 = bot.players[0].state().queue[1].id;
        let from_queue = serde_json::json!({"player":0,"control":"load","deck":0,"entry":q3});
        assert_eq!(
            status(send(from_queue.clone()).await),
            StatusCode::NO_CONTENT
        );
        assert_eq!(view()["decks"][0]["entry"]["track"]["id"], "q3");
        assert_eq!(queue_ids(&bot), ["q1", "q2"]);
        assert_eq!(status(send(from_queue).await), StatusCode::CONFLICT);
        for accepted in [
            volume,
            serde_json::json!({"player":0,"control":"eq","deck":0,"band":"mid","db":-26}),
            serde_json::json!({"player":0,"control":"filter","deck":0,"value":-0.25}),
            serde_json::json!({"player":0,"control":"tempo","deck":0,"tempo":12}),
            serde_json::json!({"player":0,"control":"crossfader","position":0.5}),
            serde_json::json!({"player":0,"control":"curve","curve":"cut"}),
            serde_json::json!({"player":0,"control":"auto_load","on":false}),
            serde_json::json!({"player":0,"control":"seek","deck":0,"position_ms":61_000}),
            serde_json::json!({"player":0,"control":"set_cue","deck":0}),
            serde_json::json!({"player":0,"control":"transition","seconds":8}),
        ] {
            assert_eq!(
                status(send(accepted.clone()).await),
                StatusCode::NO_CONTENT,
                "{accepted}"
            );
        }
        let state = view();
        let deck = &state["decks"][0];
        assert_eq!(deck["volume"], 50);
        assert_eq!(deck["eq"]["mid"], -26.0);
        assert_eq!(deck["filter"], -0.25);
        // Tempo is in percent and stays within the deck's 8 % range.
        assert_eq!(deck["tempo"], 8.0);
        assert_eq!(deck["tempo_range"], 8);
        assert_eq!(deck["position_ms"], 61_000);
        assert_eq!(deck["cue_ms"], 61_000);
        assert_eq!(state["curve"], "cut");
        assert_eq!(state["auto_load"], false);
        // The crossfader stands towards B, so the transition fades to A.
        assert_eq!(state["transition"]["to"], 0);
        assert_eq!(state["transition"]["duration_ms"], 8_000);
        assert_eq!(deck["playing"], true);
        let eject = serde_json::json!({"player":0,"control":"eject","deck":1});
        assert_eq!(status(send(eject).await), StatusCode::NO_CONTENT);
        let play = serde_json::json!({"player":0,"control":"play","deck":1,"playing":true});
        assert_eq!(status(send(play).await), StatusCode::CONFLICT);
        let sync = serde_json::json!({"player":0,"control":"sync","deck":0});
        assert_eq!(status(send(sync).await), StatusCode::CONFLICT);
        let off = serde_json::json!({"player":0,"control":"mode","on":false});
        assert_eq!(status(send(off).await), StatusCode::NO_CONTENT);
        assert!(view().is_null());
        let current = bot.players[0].state().current.clone().unwrap();
        assert_eq!(current.entry.track.id, "q3");
        assert_eq!(current.elapsed(), Duration::from_secs(61));
    }

    #[tokio::test]
    async fn cue_streams_have_their_own_admission_and_check_the_deck_first() {
        static CUES: Semaphore = Semaphore::const_new(MAX_CUE_STREAMS);
        static ROOM: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        let open =
            |token: String, stream: Listen| listen_response(bot.clone(), token, 0, stream, &CUES);
        assert_eq!(
            open(token.clone(), Listen::Cue(DECKS)).err().unwrap().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            open("invalid".into(), Listen::Cue(0)).err().unwrap().0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(CUES.available_permits(), MAX_CUE_STREAMS);
        let responses: Vec<_> = (0..MAX_CUE_STREAMS)
            .map(|index| {
                open(token.clone(), Listen::Cue(index % DECKS))
                    .ok()
                    .unwrap()
            })
            .collect();
        assert_eq!(bot.players[0].cue_listeners(), [2, 2]);
        assert_eq!(bot.players[0].listeners(), 0, "cue listeners hear no room");
        let refused = open(token.clone(), Listen::Cue(1)).err().unwrap();
        assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE);
        let room = listen_response(bot.clone(), token, 0, Listen::Room, &ROOM)
            .ok()
            .unwrap();
        assert_eq!(room.headers()["x-audio-frame-samples"], "960");
        assert_eq!(bot.players[0].listeners(), 1);
        drop(responses);
        let mut changes = bot.changes.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            while bot.players[0].cue_listeners() != [0, 0] {
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(CUES.available_permits(), MAX_CUE_STREAMS);
        drop(room);
    }

    #[tokio::test(start_paused = true)]
    async fn state_events_come_at_most_ten_times_a_second() {
        static WATCHERS: Semaphore = Semaphore::const_new(1);
        let (_directory, bot, token) = listening_bot();
        let permit = WATCHERS.try_acquire().unwrap();
        let mut stream = std::pin::pin!(state_stream(
            bot.clone(),
            token,
            bot.changes.subscribe(),
            permit
        ));
        let _first = stream.next().await.unwrap().unwrap();
        // A fader moved thirty times a second.
        let changes = bot.changes.clone();
        let mover = tokio::spawn(async move {
            loop {
                changes.notify();
                tokio::time::sleep(Duration::from_millis(33)).await;
            }
        });
        let mut last = tokio::time::Instant::now();
        for _ in 0..10 {
            let _event = stream.next().await.unwrap().unwrap();
            let now = tokio::time::Instant::now();
            assert!(now - last >= MIN_EVENT_GAP, "{:?}", now - last);
            last = now;
        }
        mover.abort();
    }

    #[tokio::test]
    async fn spectrum_ranges_are_bounded_and_bound_to_the_decks_song() {
        let (_directory, mut bot, token) = dj_bot("http://unused.invalid", 0);
        let starts = crate::preload::tests::Starts::default();
        let pool =
            crate::preload::Pool::new(1 << 30, starts.starter(), crate::beats::Beats::memory());
        Arc::get_mut(&mut bot).unwrap().preload = Some(pool.clone());
        let mut song = fixture_track("song");
        song.duration = Some(60);
        bot.players[0].enqueue(vec![song.clone()], "ada", None);
        bot.players[0].set_dj(true);
        let entry = bot.players[0].state().dj.as_ref().unwrap().decks[0]
            .track
            .as_ref()
            .unwrap()
            .entry
            .id;
        let query = |entry: u64, count: Option<usize>, view: SpectrumView| SpectrumQuery {
            player: 0,
            deck: 0,
            entry,
            view: Some(view),
            from: Some(0),
            count,
        };
        let status = |result: Result<Response, ApiError>| match result {
            Ok(response) => response.status(),
            Err(error) => error.0,
        };
        let ask =
            |query: SpectrumQuery| spectrum(State(bot.clone()), Path(token.clone()), Query(query));
        assert_eq!(
            status(ask(query(entry, None, SpectrumView::Coarse)).await),
            StatusCode::NOT_FOUND
        );
        pool.want(
            0,
            &[crate::preload::Want {
                entry,
                track: song,
                need: crate::preload::Need::Loaded,
            }],
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let decoder = starts.take(entry);
        for frame in 0..100usize {
            let samples = (0..crate::decode::FRAME)
                .flat_map(|index| {
                    let at = (frame * crate::decode::FRAME + index) as f32 / 48_000.0;
                    [(std::f32::consts::TAU * 60.0 * at).sin() * 0.5; 2]
                })
                .collect();
            decoder.sender.send(Ok(samples)).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while pool.buffer(0, entry).unwrap().spectrum().columns() < 190 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let response = ask(query(entry, Some(150), SpectrumView::Coarse))
            .await
            .ok()
            .unwrap();
        assert_eq!(response.headers()["x-spectrum-total"], "6000");
        assert_eq!(response.headers()["x-spectrum-bands"], "3");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.len(), 150 * 3);
        // The 60 Hz tone lights the low band.
        assert!(body[100 * 3] > body[100 * 3 + 2]);
        let fine = ask(query(entry, Some(10), SpectrumView::Fine))
            .await
            .ok()
            .unwrap();
        let body = axum::body::to_bytes(fine.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.len(), 10 * crate::spectrum::FINE_BANDS);
        assert_eq!(
            status(
                ask(query(
                    entry,
                    Some(MAX_SPECTRUM_COLUMNS + 1),
                    SpectrumView::Coarse
                ))
                .await
            ),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(ask(query(entry + 1, None, SpectrumView::Coarse)).await),
            StatusCode::CONFLICT,
            "an answer belongs to the song the deck plays"
        );
        let overview = spectrum_overview(
            State(bot.clone()),
            Path(token.clone()),
            Query(query(entry, None, SpectrumView::Coarse)),
        )
        .await
        .ok()
        .unwrap();
        let body = axum::body::to_bytes(overview.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.len(), OVERVIEW_COLUMNS * 3);
        drop(decoder);
    }
}
