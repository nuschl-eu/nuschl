// SPDX-License-Identifier: EUPL-1.2
//! Personal playlists: playlists of a library's configured account that Mumble
//! users own through `playlists.json` in the state directory (docs/musicbot.md,
//! "Playlists"). The file maps a library key and playlist id to the owner's
//! pseudonym, their Mumble name, the shared flag and the Favorites flag.
use crate::{import::Pseudonyms, tokens::Owner};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::OwnedMutexGuard;

pub const FILE: &str = "playlists.json";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Personal playlists of all people together.
pub const MAX_ENTRIES: usize = 10_000;
/// Personal playlists one person owns across all libraries, Favorites included.
pub const MAX_PER_PERSON: usize = 50;
/// Characters of a playlist name.
pub const MAX_NAME_CHARS: usize = 100;
/// Characters of an owner's name as stored and shown.
const MAX_OWNER_NAME_CHARS: usize = 128;
/// Songs in one personal playlist.
pub const MAX_SONGS: usize = 5_000;
/// Songs one request adds.
pub const MAX_ADD: usize = 500;
/// Bytes of one song id.
pub const MAX_ID_BYTES: usize = 128;
/// How long a change waits for an earlier change of the same playlist or person.
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// Who owns one playlist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The library's key from `library_keys.json`.
    pub library: String,
    pub id: String,
    /// The owner's pseudonym (`import::Pseudonyms`).
    pub owner: String,
    /// The owner's Mumble name when they last changed a playlist.
    pub owner_name: String,
    /// Everyone with a certificate may change the songs.
    pub shared: bool,
    /// The owner's Favorites in this library.
    pub favorites: bool,
}

#[derive(Serialize)]
struct Saving<'a> {
    version: u32,
    playlists: &'a [Entry],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    playlists: Vec<Entry>,
}

/// Owning, creating, editing and favoriting need a Mumble certificate.
pub fn can_own(owner: &Owner) -> bool {
    owner
        .certificate
        .as_deref()
        .is_some_and(|hash| !hash.is_empty())
}

/// The person asking: their pseudonym when their Mumble client has a
/// certificate, and their name.
pub struct Caller {
    pub pseudonym: Option<String>,
    pub name: String,
}

impl Caller {
    pub fn new(owner: &Owner, pseudonyms: &Pseudonyms) -> Self {
        Self {
            pseudonym: can_own(owner).then(|| pseudonyms.of(owner)),
            name: owner.name.chars().take(MAX_OWNER_NAME_CHARS).collect(),
        }
    }

    pub fn can_own(&self) -> bool {
        self.pseudonym.is_some()
    }
}

/// What one caller may do with one playlist, as the page shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Access {
    /// A person owns it through the bot.
    pub personal: bool,
    pub owner_name: Option<String>,
    pub mine: bool,
    pub shared: bool,
    pub favorites: bool,
    /// The caller may add and remove songs.
    pub editable: bool,
    /// The caller may rename, share and delete it.
    pub manageable: bool,
}

/// The owner edits and manages their playlists; people with a certificate
/// edit the songs of shared ones. Favorites keep their name and stay private.
/// Library playlists without an entry are read-only.
pub fn access(entry: Option<&Entry>, caller: &Caller) -> Access {
    let Some(entry) = entry else {
        return Access::default();
    };
    let mine = caller.pseudonym.as_deref() == Some(entry.owner.as_str());
    Access {
        personal: true,
        owner_name: Some(entry.owner_name.clone()),
        mine,
        shared: entry.shared,
        favorites: entry.favorites,
        editable: caller.can_own() && (mine || entry.shared),
        manageable: mine && !entry.favorites,
    }
}

/// A trimmed name of 1 to 100 characters without control characters.
pub fn valid_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control))
    .then(|| name.to_owned())
}

/// A song id of 1 to 128 bytes without control characters.
pub fn valid_song_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

/// The name of a person's Favorites in the library.
pub fn favorites_name(owner_name: &str) -> String {
    let name = format!("Favorites of {owner_name}");
    valid_name(&name).unwrap_or_else(|| "Favorites".to_owned())
}

pub struct Playlists {
    path: PathBuf,
    entries: Mutex<Vec<Entry>>,
    /// One lock per playlist or person with a change in progress.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Advances after every change of a personal playlist or its entry.
    revision: AtomicU64,
}

impl Playlists {
    /// Reads `path`, keeping only entries of the libraries in `keys`: a key
    /// that left `library_keys.json` never comes back.
    pub fn load(path: PathBuf, keys: &[String]) -> io::Result<Self> {
        let entries = match crate::persist::open_private(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err(io::Error::other("file exceeds 4 MiB"));
                }
                let saved: Saved = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if saved.version != VERSION {
                    return Err(io::Error::other(format!(
                        "unknown version {}",
                        saved.version
                    )));
                }
                if saved.playlists.len() > MAX_ENTRIES {
                    return Err(io::Error::other("file exceeds 10000 playlists"));
                }
                saved.playlists
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let playlists = Self {
            path,
            entries: Mutex::new(entries),
            locks: Mutex::default(),
            revision: AtomicU64::new(0),
        };
        playlists.update(|current| current.retain(|entry| keys.contains(&entry.library)))?;
        Ok(playlists)
    }

    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Changes the entries and saves them; on a failed save nothing changes.
    fn update<R>(&self, change: impl FnOnce(&mut Vec<Entry>) -> R) -> io::Result<R> {
        let mut entries = self.entries();
        let mut next = entries.clone();
        let result = change(&mut next);
        if next != *entries {
            let bytes = serde_json::to_vec(&Saving {
                version: VERSION,
                playlists: &next,
            })
            .map_err(io::Error::other)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(io::Error::other("file would exceed 4 MiB"));
            }
            crate::persist::write(&self.path, &bytes)?;
            *entries = next;
        }
        Ok(result)
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    /// Tells every viewer that personal playlists changed.
    pub fn changed(&self, changes: &crate::player::Changes) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        changes.notify();
    }

    pub fn get(&self, library: &str, id: &str) -> Option<Entry> {
        self.entries()
            .iter()
            .find(|entry| entry.library == library && entry.id == id)
            .cloned()
    }

    /// The ids of this library's personal playlists.
    pub fn ids(&self, library: &str) -> HashSet<String> {
        self.entries()
            .iter()
            .filter(|entry| entry.library == library)
            .map(|entry| entry.id.clone())
            .collect()
    }

    pub fn favorites(&self, library: &str, owner: &str) -> Option<Entry> {
        self.entries()
            .iter()
            .find(|entry| entry.library == library && entry.owner == owner && entry.favorites)
            .cloned()
    }

    /// Adds a new playlist's entry within the caps.
    pub fn insert(&self, entry: Entry) -> Result<(), Full> {
        self.update(|entries| {
            if entries.len() >= MAX_ENTRIES {
                return Err(Full::Everyone);
            }
            if entries
                .iter()
                .filter(|known| known.owner == entry.owner)
                .count()
                >= MAX_PER_PERSON
            {
                return Err(Full::Person);
            }
            entries.retain(|known| !(known.library == entry.library && known.id == entry.id));
            entries.push(entry);
            Ok(())
        })
        .map_err(Full::Storage)?
    }

    /// Whether `owner` may make another playlist now.
    pub fn room_for(&self, owner: &str) -> Result<(), Full> {
        let entries = self.entries();
        if entries.len() >= MAX_ENTRIES {
            Err(Full::Everyone)
        } else if entries.iter().filter(|entry| entry.owner == owner).count() >= MAX_PER_PERSON {
            Err(Full::Person)
        } else {
            Ok(())
        }
    }

    pub fn set_shared(&self, library: &str, id: &str, shared: bool) -> io::Result<()> {
        self.update(|entries| {
            for entry in entries
                .iter_mut()
                .filter(|entry| entry.library == library && entry.id == id)
            {
                entry.shared = shared;
            }
        })
    }

    pub fn remove(&self, library: &str, id: &str) -> io::Result<()> {
        self.update(|entries| entries.retain(|entry| !(entry.library == library && entry.id == id)))
    }

    /// Keeps the owner's current Mumble name on all their playlists; true when it changed.
    pub fn rename_owner(&self, owner: &str, name: &str) -> io::Result<bool> {
        self.update(|entries| {
            let mut changed = false;
            for entry in entries
                .iter_mut()
                .filter(|entry| entry.owner == owner && entry.owner_name != name)
            {
                entry.owner_name = name.to_owned();
                changed = true;
            }
            changed
        })
    }

    /// Drops entries of `library` that were known before the library listed
    /// its playlists (`before`) and are missing from that list. Entries made
    /// while the list was asked for stay. Returns how many went.
    pub fn prune<'a>(
        &self,
        library: &str,
        before: &HashSet<String>,
        listed: impl IntoIterator<Item = &'a str>,
    ) -> io::Result<usize> {
        let listed: HashSet<&str> = listed.into_iter().collect();
        self.update(|entries| {
            let count = entries.len();
            entries.retain(|entry| {
                entry.library != library
                    || !before.contains(&entry.id)
                    || listed.contains(entry.id.as_str())
            });
            count - entries.len()
        })
    }

    /// Waits for earlier changes of the same `key` (see [`playlist_lock`] and
    /// [`person_lock`]). The guard belongs to the work it protects: move it
    /// into the blocking library call so it lasts until that call ends.
    pub async fn lock(&self, key: String) -> Option<OwnedMutexGuard<()>> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Locks nobody holds or waits for are dropped, so the map stays small.
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            locks.entry(key).or_default().clone()
        };
        tokio::time::timeout(LOCK_WAIT, lock.lock_owned())
            .await
            .ok()
    }
}

pub fn playlist_lock(library: &str, id: &str) -> String {
    format!("playlist\0{library}\0{id}")
}

pub fn person_lock(owner: &str) -> String {
    format!("person\0{owner}")
}

/// Why a playlist cannot be made.
#[derive(Debug)]
pub enum Full {
    Person,
    Everyone,
    Storage(io::Error),
}

#[cfg(test)]
mod tests;
