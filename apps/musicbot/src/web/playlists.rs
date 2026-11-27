// SPDX-License-Identifier: EUPL-1.2
//! Playlists on the web page: the library's playlists with what the caller
//! may do with each, and the JSON changes of personal playlists
//! (docs/musicbot.md, "Playlists" and "Web API"). Every change of one playlist
//! or one person's set of playlists waits for the one before; the lock and the
//! revision update belong to the admitted library call, so a caller that goes
//! away cannot release or hide work still running.
use super::{ApiError, Checked, ItemQuery, LibraryQuery, ask, ask_checked, bad, owner};
use crate::{
    Bot,
    playlists::{self, Access, Caller, Entry, Full, MAX_ADD, MAX_SONGS},
    subsonic::{Playlist, Track},
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;

const NO_CERTIFICATE: &str = "Owning and editing playlists needs a Mumble certificate. Set one up in Mumble, then send !web for a new link.";
const LIBRARY_PLAYLIST: &str = "This playlist comes from the library and is read-only here.";
const OWNER_ONLY: &str = "Only the owner renames, shares or deletes this playlist.";
const FAVORITES_FIXED: &str = "Favorites keeps its name and stays private.";
const PRIVATE: &str = "Only the owner changes this playlist while it is private.";
const BUSY: &str = "Somebody is changing this playlist right now. Try again in a moment.";
const STALE: &str = "The playlist changed meanwhile. Reload it and try again.";
const STORAGE: &str = "The bot could not save who owns the playlist.";

fn forbidden(message: &str) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, message.to_owned())
}

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::CONFLICT, message.into())
}

fn name_rule() -> ApiError {
    bad(format!(
        "A playlist name has 1 to {} characters.",
        playlists::MAX_NAME_CHARS
    ))
}

fn full_playlist() -> ApiError {
    conflict(format!("A playlist holds at most {MAX_SONGS} songs."))
}

fn full(full: Full) -> ApiError {
    match full {
        Full::Person => conflict(format!(
            "You have {} playlists, the most one person can have. Delete one to make another.",
            playlists::MAX_PER_PERSON
        )),
        Full::Everyone => conflict(format!(
            "The bot keeps at most {} personal playlists, and that many exist.",
            playlists::MAX_ENTRIES
        )),
        Full::Storage(error) => {
            tracing::error!("{}: {error}", playlists::FILE);
            ApiError(StatusCode::INTERNAL_SERVER_ERROR, STORAGE.to_owned())
        }
    }
}

fn caller(bot: &Bot, token: &str) -> Result<Caller, ApiError> {
    Ok(Caller::new(&owner(bot, token)?, &bot.pseudonyms))
}

/// The pseudonym of a caller who may own playlists.
fn certified(caller: &Caller) -> Result<String, ApiError> {
    caller
        .pseudonym
        .clone()
        .ok_or_else(|| forbidden(NO_CERTIFICATE))
}

fn library_key(bot: &Bot, library: usize) -> Result<String, ApiError> {
    super::known_library(bot, library)?;
    Ok(bot.libraries[library].key().to_owned())
}

async fn lock(bot: &Bot, key: String) -> Result<OwnedMutexGuard<()>, ApiError> {
    bot.playlists
        .lock(key)
        .await
        .ok_or_else(|| ApiError(StatusCode::SERVICE_UNAVAILABLE, BUSY.to_owned()))
}

/// Keeps the caller's Mumble name on their playlists.
fn refresh_owner(bot: &Bot, caller: &Caller) {
    let Some(owner) = &caller.pseudonym else {
        return;
    };
    match bot.playlists.rename_owner(owner, &caller.name) {
        Ok(true) => bot.playlists.changed(&bot.changes),
        Ok(false) => {}
        Err(error) => tracing::warn!("{}: {error}", playlists::FILE),
    }
}

/// The entry of a personal playlist the caller may add songs to and remove them from.
fn editable(bot: &Bot, key: &str, id: &str, caller: &Caller) -> Result<Entry, ApiError> {
    certified(caller)?;
    let entry = bot
        .playlists
        .get(key, id)
        .ok_or_else(|| forbidden(LIBRARY_PLAYLIST))?;
    if playlists::access(Some(&entry), caller).editable {
        Ok(entry)
    } else {
        Err(forbidden(PRIVATE))
    }
}

/// The entry of a personal playlist the caller may rename, share and delete.
fn manageable(bot: &Bot, key: &str, id: &str, caller: &Caller) -> Result<Entry, ApiError> {
    certified(caller)?;
    let entry = bot
        .playlists
        .get(key, id)
        .ok_or_else(|| forbidden(LIBRARY_PLAYLIST))?;
    let access = playlists::access(Some(&entry), caller);
    if access.manageable {
        Ok(entry)
    } else if access.mine {
        Err(forbidden(FAVORITES_FIXED))
    } else {
        Err(forbidden(OWNER_ONLY))
    }
}

/// A playlist with what the caller may do with it.
#[derive(Serialize)]
pub(super) struct PlaylistItem {
    #[serde(flatten)]
    playlist: Playlist,
    #[serde(flatten)]
    access: Access,
}

fn item(bot: &Bot, key: &str, playlist: Playlist, caller: &Caller) -> PlaylistItem {
    let access = playlists::access(bot.playlists.get(key, &playlist.id).as_ref(), caller);
    PlaylistItem { playlist, access }
}

#[derive(Serialize)]
pub(super) struct PlaylistList {
    playlists: Vec<PlaylistItem>,
}

/// The library's playlists. Entries of personal playlists the library no
/// longer lists go.
pub(super) async fn list(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<LibraryQuery>,
) -> Result<Json<PlaylistList>, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, query.library)?;
    let before = bot.playlists.ids(&key);
    let (owned, pruning) = (bot.clone(), key.clone());
    let playlists = ask(&bot, query.library, move |library| {
        let playlists = library.playlists()?;
        let listed = playlists.iter().map(|playlist| playlist.id.as_str());
        match owned.playlists.prune(&pruning, &before, listed) {
            Ok(0) => {}
            Ok(_) => owned.playlists.changed(&owned.changes),
            Err(error) => tracing::warn!("{}: {error}", playlists::FILE),
        }
        Ok(playlists)
    })
    .await?;
    let playlists = playlists
        .into_iter()
        .map(|playlist| item(&bot, &key, playlist, &caller))
        .collect();
    Ok(Json(PlaylistList { playlists }))
}

#[derive(Serialize)]
pub(super) struct PlaylistView {
    playlist: PlaylistItem,
    songs: Vec<Track>,
}

pub(super) async fn detail(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<PlaylistView>, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, query.library)?;
    let (playlist, songs) = ask(&bot, query.library, move |library| {
        library.playlist(&query.id)
    })
    .await?;
    Ok(Json(PlaylistView {
        playlist: item(&bot, &key, playlist, &caller),
        songs,
    }))
}

/// Makes the library playlist and its entry in one admitted call that holds
/// `person` (the owner's lock) until it ends, and hands the lock back.
async fn create_owned(
    bot: &Arc<Bot>,
    library: usize,
    name: String,
    mut entry: Entry,
    person: OwnedMutexGuard<()>,
) -> Result<(Playlist, Entry, OwnedMutexGuard<()>), ApiError> {
    let owned = bot.clone();
    ask_checked(bot, library, move |library| {
        let playlist = library.create_playlist(&name)?;
        entry.id = playlist.id.clone();
        if let Err(failure) = owned.playlists.insert(entry.clone()) {
            if let Err(error) = library.delete_playlist(&playlist.id) {
                tracing::warn!("{}: playlist {}: {error}", library.name, playlist.id);
            }
            return Err(Checked::Refused(full(failure)));
        }
        owned.playlists.changed(&owned.changes);
        Ok((playlist, entry, person))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewPlaylist {
    library: usize,
    name: String,
    #[serde(default)]
    shared: bool,
}

/// Makes a personal playlist in the library, owned by the caller.
pub(super) async fn create(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Json(body): Json<NewPlaylist>,
) -> Result<Json<PlaylistItem>, ApiError> {
    let caller = caller(&bot, &token)?;
    let owner = certified(&caller)?;
    let key = library_key(&bot, body.library)?;
    let name = playlists::valid_name(&body.name).ok_or_else(name_rule)?;
    let person = lock(&bot, playlists::person_lock(&owner)).await?;
    bot.playlists.room_for(&owner).map_err(full)?;
    refresh_owner(&bot, &caller);
    let entry = Entry {
        library: key,
        id: String::new(),
        owner,
        owner_name: caller.name.clone(),
        shared: body.shared,
        favorites: false,
    };
    let (playlist, entry, _person) = create_owned(&bot, body.library, name, entry, person).await?;
    tracing::info!("{} made playlist {}", caller.name, playlist.id);
    Ok(Json(PlaylistItem {
        access: playlists::access(Some(&entry), &caller),
        playlist,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Renaming {
    library: usize,
    id: String,
    name: String,
}

pub(super) async fn rename(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Json(body): Json<Renaming>,
) -> Result<StatusCode, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, body.library)?;
    let name = playlists::valid_name(&body.name).ok_or_else(name_rule)?;
    certified(&caller)?;
    let guard = lock(&bot, playlists::playlist_lock(&key, &body.id)).await?;
    manageable(&bot, &key, &body.id, &caller)?;
    refresh_owner(&bot, &caller);
    let owned = bot.clone();
    ask_checked(&bot, body.library, move |library| {
        let _guard = guard;
        let result = library.update_playlist(&body.id, Some(&name), None);
        owned.playlists.changed(&owned.changes);
        Ok(result?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Sharing {
    library: usize,
    id: String,
    shared: bool,
}

/// Lets everyone with a certificate edit the songs, or only the owner.
pub(super) async fn share(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Json(body): Json<Sharing>,
) -> Result<StatusCode, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, body.library)?;
    certified(&caller)?;
    let _guard = lock(&bot, playlists::playlist_lock(&key, &body.id)).await?;
    manageable(&bot, &key, &body.id, &caller)?;
    refresh_owner(&bot, &caller);
    bot.playlists
        .set_shared(&key, &body.id, body.shared)
        .map_err(|error| full(Full::Storage(error)))?;
    bot.playlists.changed(&bot.changes);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Deletion {
    library: usize,
    id: String,
}

pub(super) async fn delete(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Json(body): Json<Deletion>,
) -> Result<StatusCode, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, body.library)?;
    certified(&caller)?;
    let guard = lock(&bot, playlists::playlist_lock(&key, &body.id)).await?;
    manageable(&bot, &key, &body.id, &caller)?;
    refresh_owner(&bot, &caller);
    let owned = bot.clone();
    ask_checked(&bot, body.library, move |library| {
        let _guard = guard;
        let result = library.delete_playlist(&body.id);
        if result.is_ok()
            && let Err(error) = owned.playlists.remove(&key, &body.id)
        {
            tracing::warn!("{}: {error}", playlists::FILE);
        }
        owned.playlists.changed(&owned.changes);
        Ok(result?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Addition {
    library: usize,
    id: String,
    /// Song ids of the playlist's library.
    songs: Option<Vec<String>>,
    /// An album of the playlist's library, all its songs.
    album: Option<String>,
    /// The song this queue plays.
    player: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct Added {
    added: usize,
}

/// What to add, checked before any library call.
enum Songs {
    Ids(Vec<String>),
    Album(String),
}

fn songs_to_add(bot: &Bot, body: &Addition) -> Result<Songs, ApiError> {
    match (&body.songs, &body.album, body.player) {
        (Some(songs), None, None) => {
            if songs.is_empty() || songs.len() > MAX_ADD {
                return Err(bad(format!("Add 1 to {MAX_ADD} songs at a time.")));
            }
            if !songs.iter().all(|song| playlists::valid_song_id(song)) {
                return Err(bad("invalid song id"));
            }
            Ok(Songs::Ids(songs.clone()))
        }
        (None, Some(album), None) if playlists::valid_song_id(album) => {
            Ok(Songs::Album(album.clone()))
        }
        (None, Some(_), None) => Err(bad("invalid album id")),
        (None, None, Some(player)) => {
            let player = bot
                .players
                .get(player)
                .ok_or_else(|| bad("no such queue"))?;
            let state = player.state();
            let playing = state
                .now_playing()
                .ok_or_else(|| bad("Nothing is playing."))?;
            let identity = bot.libraries[body.library].identity();
            if playing.source.as_ref() != Some(&identity) {
                return Err(bad(
                    "The playing song comes from another library than this playlist.",
                ));
            }
            Ok(Songs::Ids(vec![playing.entry.track.id.clone()]))
        }
        _ => Err(bad("Name exactly one of songs, album or player.")),
    }
}

/// Appends songs, an album or the playing song to a playlist the caller edits.
pub(super) async fn add(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Json(body): Json<Addition>,
) -> Result<Json<Added>, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, body.library)?;
    certified(&caller)?;
    let songs = songs_to_add(&bot, &body)?;
    let guard = lock(&bot, playlists::playlist_lock(&key, &body.id)).await?;
    editable(&bot, &key, &body.id, &caller)?;
    refresh_owner(&bot, &caller);
    let owned = bot.clone();
    let id = body.id;
    let added = ask_checked(&bot, body.library, move |library| {
        let _guard = guard;
        let songs: Vec<String> = match songs {
            Songs::Ids(ids) => ids,
            Songs::Album(album) => library
                .album(&album)?
                .1
                .into_iter()
                .map(|song| song.id)
                .collect(),
        };
        if songs.len() > MAX_ADD {
            return Err(Checked::Refused(bad(format!(
                "Add 1 to {MAX_ADD} songs at a time."
            ))));
        }
        let (_, present) = library.playlist(&id)?;
        if present.len() + songs.len() > MAX_SONGS {
            return Err(Checked::Refused(full_playlist()));
        }
        if songs.is_empty() {
            return Ok(0);
        }
        let result = library.add_to_playlist(&id, &songs);
        owned.playlists.changed(&owned.changes);
        result?;
        Ok(songs.len())
    })
    .await?;
    Ok(Json(Added { added }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Removal {
    library: usize,
    id: String,
    /// The song's position in the playlist as the caller saw it.
    index: usize,
    /// The song the caller saw there.
    song: String,
}

/// Removes one song after checking that it is still at that position.
pub(super) async fn remove(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Json(body): Json<Removal>,
) -> Result<StatusCode, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, body.library)?;
    certified(&caller)?;
    let guard = lock(&bot, playlists::playlist_lock(&key, &body.id)).await?;
    editable(&bot, &key, &body.id, &caller)?;
    refresh_owner(&bot, &caller);
    let owned = bot.clone();
    ask_checked(&bot, body.library, move |library| {
        let _guard = guard;
        let (_, songs) = library.playlist(&body.id)?;
        if songs.get(body.index).map(|song| song.id.as_str()) != Some(body.song.as_str()) {
            return Err(Checked::Refused(conflict(STALE)));
        }
        let result = library.remove_from_playlist(&body.id, &[body.index]);
        owned.playlists.changed(&owned.changes);
        Ok(result?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub(super) struct FavoriteView {
    favorite: bool,
}

/// Whether the song is in the caller's Favorites of its library.
pub(super) async fn favorite(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<FavoriteView>, ApiError> {
    let caller = caller(&bot, &token)?;
    let key = library_key(&bot, query.library)?;
    let Some(entry) = caller
        .pseudonym
        .and_then(|owner| bot.playlists.favorites(&key, &owner))
    else {
        return Ok(Json(FavoriteView { favorite: false }));
    };
    let favorite = ask(&bot, query.library, move |library| {
        let (_, songs) = library.playlist(&entry.id)?;
        Ok(songs.iter().any(|song| song.id == query.id))
    })
    .await?;
    Ok(Json(FavoriteView { favorite }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Favoriting {
    library: usize,
    /// The song.
    id: String,
    favorite: bool,
}

/// Puts a song into the caller's Favorites of its library, made on first
/// use, or takes every copy of it out.
pub(super) async fn set_favorite(
    State(bot): State<Arc<Bot>>,
    Path(token): Path<String>,
    Json(body): Json<Favoriting>,
) -> Result<Json<FavoriteView>, ApiError> {
    let caller = caller(&bot, &token)?;
    let owner = certified(&caller)?;
    let key = library_key(&bot, body.library)?;
    if !playlists::valid_song_id(&body.id) {
        return Err(bad("invalid song id"));
    }
    let person = lock(&bot, playlists::person_lock(&owner)).await?;
    refresh_owner(&bot, &caller);
    let (entry, person) = match bot.playlists.favorites(&key, &owner) {
        Some(entry) => (entry, person),
        None if !body.favorite => return Ok(Json(FavoriteView { favorite: false })),
        None => {
            bot.playlists.room_for(&owner).map_err(full)?;
            let entry = Entry {
                library: key.clone(),
                id: String::new(),
                owner,
                owner_name: caller.name.clone(),
                shared: false,
                favorites: true,
            };
            let name = playlists::favorites_name(&caller.name);
            let (_, entry, person) = create_owned(&bot, body.library, name, entry, person).await?;
            (entry, person)
        }
    };
    let guard = lock(&bot, playlists::playlist_lock(&key, &entry.id)).await?;
    let owned = bot.clone();
    let favorite = ask_checked(&bot, body.library, move |library| {
        let _guards = (person, guard);
        let (_, songs) = library.playlist(&entry.id)?;
        let positions: Vec<usize> = songs
            .iter()
            .enumerate()
            .filter(|(_, song)| song.id == body.id)
            .map(|(index, _)| index)
            .collect();
        let result = match (body.favorite, positions.is_empty()) {
            (true, true) if songs.len() >= MAX_SONGS => {
                return Err(Checked::Refused(full_playlist()));
            }
            (true, true) => library.add_to_playlist(&entry.id, &[body.id]),
            (false, false) => library.remove_from_playlist(&entry.id, &positions),
            _ => return Ok(body.favorite),
        };
        owned.playlists.changed(&owned.changes);
        result?;
        Ok(body.favorite)
    })
    .await?;
    Ok(Json(FavoriteView { favorite }))
}

#[cfg(test)]
mod tests;
