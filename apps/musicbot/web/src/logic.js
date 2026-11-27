// SPDX-License-Identifier: EUPL-1.2
// Pure helpers without DOM or Svelte dependencies, tested in logic.test.js.

/** Formats seconds as `3:07` or `1:02:03`. Invalid input gives an empty string. */
export function formatDuration(seconds) {
  if (typeof seconds !== "number" || !Number.isFinite(seconds)) return "";
  const total = Math.max(0, Math.floor(seconds));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

/** Sum of the known durations of tracks, in seconds. */
export function totalDuration(tracks) {
  let sum = 0;
  for (const track of tracks) if (typeof track?.duration === "number") sum += track.duration;
  return sum;
}

/**
 * Position in ms shown for `current`: `position_ms` plus the time since the
 * snapshot arrived while `advancing`, never below 0 and capped at the duration
 * (seconds, may be null).
 */
export function interpolatePosition(positionMs, advancing, receivedAt, now, durationSeconds) {
  let ms = positionMs + (advancing ? Math.max(0, now - receivedAt) : 0);
  if (typeof durationSeconds === "number" && durationSeconds > 0) ms = Math.min(ms, durationSeconds * 1000);
  return Math.max(0, ms);
}

export const clamp = (value, min, max) => Math.min(max, Math.max(min, value));

/** Fraction 0..1 of a pointer at `clientX` over a box starting at `left` with `width`. */
export function pointerFraction(clientX, left, width) {
  if (!(width > 0)) return 0;
  return clamp((clientX - left) / width, 0, 1);
}

/**
 * New slider value in ms for a key on the progress slider, or null when the
 * key does nothing. Arrows move 5 s, Page keys 30 s, Home/End jump to the ends.
 */
export function seekKeyTarget(key, valueMs, maxMs) {
  const steps = { ArrowLeft: -5000, ArrowDown: -5000, ArrowRight: 5000, ArrowUp: 5000, PageDown: -30000, PageUp: 30000 };
  if (key === "Home") return 0;
  if (key === "End") return maxMs;
  if (key in steps) return clamp(valueMs + steps[key], 0, maxMs);
  return null;
}

/**
 * Gap index (0..length) for a pointer over the row at `rowIndex`: the upper
 * half of a row means before it, the lower half after it.
 */
export function gapIndex(rowIndex, offsetY, rowHeight) {
  return rowIndex + (offsetY >= rowHeight / 2 ? 1 : 0);
}

/**
 * True when the server has caught up with a seek to `targetMs` on entry
 * `entryId`: the song changed or the reported position is within 1.5 s of the target.
 */
export function seekSettled(targetMs, entryId, current, serverMs) {
  if (!current || current.entry.id !== entryId) return true;
  return Math.abs(serverMs - targetMs) < 1500;
}

/**
 * Index for the `move` command when the entry at `fromIndex` is dropped into
 * gap `gap` (0..length, counted in the list WITH the entry). The command wants
 * the index in the list WITHOUT the entry, so gaps below the entry shift up by
 * one. Returns null when the drop leaves the order unchanged.
 */
export function moveTarget(fromIndex, gap, length) {
  if (fromIndex < 0 || fromIndex >= length) return null;
  const g = clamp(gap, 0, length);
  const target = g > fromIndex ? g - 1 : g;
  return target === fromIndex ? null : target;
}

/** Copy of `list` with the item at `fromIndex` moved to `target` (index in the list without it, clamped). */
export function applyMove(list, fromIndex, target) {
  const rest = list.slice();
  const [item] = rest.splice(fromIndex, 1);
  if (item === undefined) return list.slice();
  rest.splice(clamp(target, 0, rest.length), 0, item);
  return rest;
}

/** Target index for moving the entry at `index` one step up (-1) or down (+1), or null at the ends. */
export function stepTarget(index, delta, length) {
  const target = index + delta;
  return target < 0 || target >= length ? null : target;
}

export const ALBUM_LISTS = [
  ["newest", "Recently added"],
  ["recent", "Recently played"],
  ["frequent", "Most played"],
  ["random", "Random"],
  ["alphabeticalByName", "By name"],
  ["starred", "Starred"],
];
const LIST_NAMES = new Set(ALBUM_LISTS.map(([name]) => name));

function libraryIndex(text) {
  const n = Number(text);
  return Number.isInteger(n) && n >= 0 ? n : 0;
}

/**
 * Parses a location hash into a route object:
 * `{ view: "queue" | "channels" | "history" | "dj" }`, `{ view: "albums", library, list }`,
 * `{ view: "artists" | "playlists" | "genres", library }`, `{ view: "genre", library, name }`,
 * `{ view: "album" | "artist" | "playlist" | "song" | "similar", library, id, namespace? }`,
 * `{ view: "path", library, from, to, namespace? }`, `{ view: "search", q }`,
 * `{ view: "add", q }`, `{ view: "addAlbum", id }`, `{ view: "requests" }`.
 * A genre route without a name gives the genres view.
 * A shared link (`library_key=`, see shareRoute) adds `sharedLibrary: key` to
 * library routes instead of a namespace; adoptSharedRoute resolves it.
 * Unknown hashes give the queue view; an invalid import album id gives the add view.
 */
export function parseRoute(hash) {
  const text = (hash || "").replace(/^#/, "");
  const question = text.indexOf("?");
  const path = question < 0 ? text : text.slice(0, question);
  const params = new URLSearchParams(question < 0 ? "" : text.slice(question + 1));
  const parts = path.split("/").filter((part) => part !== "");
  const [view, library, id, to] = parts;
  let decoded;
  let decodedTo;
  try {
    decoded = id === undefined ? undefined : decodeURIComponent(id);
    decodedTo = to === undefined ? undefined : decodeURIComponent(to);
  } catch {
    return { view: "queue" };
  }
  const shared = params.has("library_key") ? { sharedLibrary: params.get("library_key") } : null;
  const namespace = shared ?? (params.has("namespace") ? { namespace: params.get("namespace") } : {});
  switch (view) {
    case "channels":
    case "history":
    case "requests":
    case "dj":
      return { view };
    case "albums": {
      const list = params.get("list");
      return { view, library: libraryIndex(params.get("library")), list: LIST_NAMES.has(list) ? list : "newest", ...shared };
    }
    case "artists":
    case "playlists":
    case "genres":
      return { view, library: libraryIndex(params.get("library")), ...shared };
    case "genre": {
      const name = params.get("name");
      const index = libraryIndex(params.get("library"));
      return name ? { view, library: index, name, ...shared } : { view: "genres", library: index, ...shared };
    }
    case "album":
    case "artist":
    case "playlist":
    case "song":
    case "similar":
      if (decoded === undefined || parts.length !== 3) return { view: "queue" };
      return { view, library: libraryIndex(library), id: decoded, ...namespace };
    case "path":
      if (decodedTo === undefined || parts.length !== 4) return { view: "queue" };
      return { view, library: libraryIndex(library), from: decoded, to: decodedTo, ...namespace };
    case "search":
      return { view, q: params.get("q") ?? "" };
    case "add":
      if (parts.length === 1) return { view, q: params.get("q") ?? "" };
      if (parts.length === 3 && library === "album" && isImportId(decoded)) return { view: "addAlbum", id: decoded };
      return { view, q: "" };
    default:
      return { view: "queue" };
  }
}

/** Builds the hash (with leading `#`) for a route object; the inverse of parseRoute. */
export function buildRoute(route) {
  switch (route.view) {
    case "history":
      return "#/history";
    case "dj":
      return "#/dj";
    case "requests":
      return "#/requests";
    case "channels":
      return "#/channels";
    case "albums":
      return `#/albums?${new URLSearchParams({ library: String(route.library ?? 0), list: route.list ?? "newest" })}`;
    case "artists":
    case "playlists":
    case "genres":
      return `#/${route.view}?library=${route.library ?? 0}`;
    case "genre":
      return `#/genre?${new URLSearchParams({ library: String(route.library ?? 0), name: route.name ?? "" })}`;
    case "album":
    case "artist":
    case "playlist":
    case "song":
    case "similar":
      return `#/${route.view}/${route.library ?? 0}/${encodeURIComponent(route.id)}${namespaceQuery(route)}`;
    case "path":
      return `#/path/${route.library ?? 0}/${encodeURIComponent(route.from)}/${encodeURIComponent(route.to)}${namespaceQuery(route)}`;
    case "search":
      return `#/search?${new URLSearchParams({ q: route.q ?? "" })}`;
    case "add":
      return `#/add?${new URLSearchParams({ q: route.q ?? "" })}`;
    case "addAlbum":
      return `#/add/album/${encodeURIComponent(route.id)}`;
    default:
      return "#/queue";
  }
}

function namespaceQuery(route) {
  return route.namespace == null ? "" : `?${new URLSearchParams({ namespace: route.namespace })}`;
}

/** The browse view to show for `route` in another library. */
export function routeForLibrary(route, library) {
  switch (route.view) {
    case "albums":
    case "genres":
    case "genre":
      return { ...route, library };
    case "artists":
    case "artist":
      return { view: "artists", library };
    case "playlists":
    case "playlist":
      return { view: "playlists", library };
    case "album":
    case "song":
      return { view: "albums", library, list: "newest" };
    case "similar":
    case "path":
      return { view: "queue" };
    default:
      return route;
  }
}

/** Keep a selected library by key (`library_keys`) only when that key is unambiguous in both namespaces. */
export function libraryForNamespace(previous, current, index) {
  const name = previous[index];
  if (name === undefined || previous.indexOf(name) !== previous.lastIndexOf(name)) return 0;
  const next = current.indexOf(name);
  return next >= 0 && next === current.lastIndexOf(name) ? next : 0;
}

const DETAIL_VIEWS = new Set(["album", "artist", "playlist", "song", "similar", "path"]);

/** Detail IDs in an unproved or expired namespace must return to their safe list. */
export function routeForNamespace(route, library, namespace) {
  if (DETAIL_VIEWS.has(route.view) && (route.library !== library || !namespace || route.namespace !== namespace)) {
    return routeForLibrary(route, library);
  }
  return typeof route.library === "number" && route.library !== library ? { ...route, library } : route;
}

/** The index of `key` in `keys`, or -1 when it is missing or ambiguous. */
function uniqueLibraryIndex(keys, key) {
  const index = keys.indexOf(key);
  return index >= 0 && index === keys.lastIndexOf(key) ? index : -1;
}

/**
 * The hash for sharing `route` with other viewers: no namespace, and a route
 * with a library carries its key (`library_key=`, from the state's
 * `library_keys`) because each viewer's indexes may differ. The key changes
 * with the library's name or URL, so an old link never sends its item ID to a
 * different backend. Null when the library index is invalid or its key is
 * shared with another library.
 */
export function shareRoute(route, libraryKeys) {
  const { namespace: _namespace, sharedLibrary: _shared, ...rest } = route;
  const hash = buildRoute(rest);
  if (typeof rest.library !== "number") return hash;
  const key = libraryKeys[rest.library];
  if (key === undefined || uniqueLibraryIndex(libraryKeys, key) !== rest.library) return null;
  return `${hash}${hash.includes("?") ? "&" : "?"}${new URLSearchParams({ library_key: key })}`;
}

/** The address bar hash for `route`: the share format, or (ambiguous library key) the route without namespace. */
export function addressRoute(route, libraryKeys) {
  const { namespace: _namespace, sharedLibrary: _shared, ...rest } = route;
  return shareRoute(rest, libraryKeys) ?? buildRoute(rest);
}

/**
 * The page's public root directory, without query or fragment. Defence in
 * depth: a legacy `/u/<token>/...` page path is cut at its first `/u/`.
 */
export function shareRoot(href) {
  const url = new URL(href);
  const legacy = /^(.*?\/)u\/[^/]+(?:\/.*)?$/.exec(url.pathname);
  return `${url.origin}${legacy ? legacy[1] : url.pathname.replace(/[^/]*$/, "")}`;
}

/** The page URL for `route` that works for every viewer with their own link, or null when it cannot be shared. */
export function shareUrl(href, route, libraryKeys) {
  const hash = shareRoute(route, libraryKeys);
  return hash === null ? null : `${shareRoot(href)}${hash}`;
}

/**
 * Resolves a shared route's library key in this viewer's libraries once
 * `namespace` exists. A missing, unknown or duplicated key falls back to the
 * first library's safe list.
 */
export function adoptSharedRoute(route, libraryKeys, namespace) {
  if (route.sharedLibrary == null || !namespace) return route;
  const { sharedLibrary, ...rest } = route;
  const library = uniqueLibraryIndex(libraryKeys, sharedLibrary);
  if (library < 0) return routeForLibrary(rest, 0);
  return DETAIL_VIEWS.has(rest.view) ? { ...rest, library, namespace } : { ...rest, library };
}

/** The shared Add/drag boundary: a captured namespace and a currently valid library index. */
export function browseItemIsCurrent(item, namespace, libraryCount) {
  return typeof namespace === "string" && namespace !== "" && item?.namespace === namespace
    && Number.isInteger(item.library) && item.library >= 0 && item.library < libraryCount;
}

/** An OpenSubsonic similarity (0..1) as an integer percent, or null when the server gave none. */
export function similarityPercent(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1 ? Math.round(value * 100) : null;
}

/** Failed loads of one cover after which it stays a placeholder. */
export const COVER_RETRIES = 6;

/**
 * Milliseconds to wait before loading a cover again after `failures` failed
 * loads: doubling from 250 up to 5 s (the bot refuses covers while its
 * artwork worker is busy), or null once it has failed `COVER_RETRIES` times.
 */
export function coverRetryDelay(failures) {
  if (!Number.isInteger(failures) || failures < 1 || failures > COVER_RETRIES) return null;
  return Math.min(5000, 250 * 2 ** (failures - 1));
}

/** The now-playing track id a sonic path to `song` can start from, or null when there is no such path. */
export function sonicPathStart(playing, song) {
  return playing && song && playing.library === song.library && typeof playing.id === "string" && playing.id !== "" && playing.id !== song.id ? playing.id : null;
}

/**
 * Calls `fn` once `ms` after the last call, with the last arguments.
 * `flush()` runs a pending call now, `cancel()` drops it.
 */
export function debounce(fn, ms) {
  let timer = null;
  let pending = null;
  const debounced = (...args) => {
    pending = args;
    clearTimeout(timer);
    timer = setTimeout(debounced.flush, ms);
  };
  debounced.flush = () => {
    clearTimeout(timer);
    timer = null;
    if (pending === null) return;
    const args = pending;
    pending = null;
    fn(...args);
  };
  debounced.cancel = () => {
    clearTimeout(timer);
    timer = null;
    pending = null;
  };
  debounced.pending = () => pending !== null;
  return debounced;
}

/**
 * localStorage key for the selected queue, per page path (normally the bot's
 * public root). The path carries no token, so the choice belongs to this
 * browser and root and outlives a change of remembered link.
 */
export function queueStorageKey(pathname) {
  return `nuschl-musicbot:queue:${pathname}`;
}

/** Index of the queue to select from a stored value, clamped to the available queues. */
export function selectedQueue(stored, count) {
  const n = Number(stored);
  if (!Number.isInteger(n) || n < 0 || count <= 0) return 0;
  return Math.min(n, count - 1);
}

export const DRAG_TYPE = "application/x-nuschl-musicbot";
export const DRAG_ENTRY_TYPE = "application/x-nuschl-musicbot-entry";
const KINDS = new Set(["song", "album", "playlist"]);

/** JSON for a library item dragged into the queue, carrying its originating namespace. */
export function encodeDragItem(kind, library, id, namespace) {
  return JSON.stringify({ kind, library, id, namespace });
}

/** Parses dragged JSON back into an item with its originating namespace, or null for invalid data. */
export function decodeDragItem(text) {
  let value;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (!value || !KINDS.has(value.kind) || !Number.isInteger(value.library) || value.library < 0) return null;
  if (typeof value.id !== "string" || value.id === "" || typeof value.namespace !== "string" || value.namespace === "") return null;
  return { kind: value.kind, library: value.library, id: value.id, namespace: value.namespace };
}

/** True when a keyboard event target is a control that handles Space itself. */
export function handlesSpace(target) {
  if (!target || typeof target.closest !== "function") return false;
  return target.closest("input, textarea, select, button, a[href], [contenteditable=''], [contenteditable='true'], [role='slider']") !== null;
}

/** Merge an authoritative history page without discarding already loaded older plays. */
export function mergeHistory(current, page, older = false) {
  const retained = current.items.filter((item) => page.oldest_id !== null && item.id >= page.oldest_id);
  const items = [...new Map([...retained, ...page.items].map((item) => [item.id, item])).values()]
    .sort((a, b) => b.id - a.id).slice(0, 1000);
  const last = page.items.at(-1)?.id;
  const joins = last !== undefined && retained.some((item) => item.id >= last - 1);
  let next_before = older || !joins ? page.next_before : current.next_before;
  if (items.length === 1000 || (next_before !== null && next_before <= page.oldest_id)) next_before = null;
  return { items, next_before, oldest_id: page.oldest_id, revision: page.revision };
}

/** True for an import service id: exactly 16 lowercase hex characters. */
export function isImportId(id) {
  return typeof id === "string" && /^[0-9a-f]{16}$/.test(id);
}

const RELEASE_TYPES = { album: "Album", ep: "EP", single: "Single", compilation: "Compilation" };

/** Display name of an import service release type, or an empty string for an unknown one. */
export function releaseTypeLabel(type) {
  return Object.hasOwn(RELEASE_TYPES, type) ? RELEASE_TYPES[type] : "";
}

const count = (value) => (Number.isInteger(value) && value >= 0 ? value : 0);

/** The status line of an import request. */
export function requestStatusText(request) {
  const total = count(request?.tracks_total);
  switch (request?.status) {
    case "queued":
      return "Waiting to start";
    case "downloading":
      return `Adding · ${count(request.tracks_done)} of ${total} tracks`;
    case "handed_over": {
      const failed = Math.min(count(request.tracks_failed), total);
      return failed === 0
        ? "Added. Shows up in the library within a few minutes."
        : `Added ${total - failed} of ${total} tracks. They show up in the library within a few minutes.`;
    }
    case "failed":
      return "Could not be added.";
    default:
      return "";
  }
}

/** Progress of a request that is being added as `{ value, max }`, or null for any other state. */
export function requestProgress(request) {
  if (request?.status !== "downloading") return null;
  const max = count(request.tracks_total);
  return max > 0 ? { value: Math.min(count(request.tracks_done), max), max } : null;
}

/** Tracks still available in an import budget `{ limit, used }`, or null when it is malformed. */
function remaining(budget) {
  const { limit, used } = budget ?? {};
  return Number.isInteger(limit) && Number.isInteger(used) && limit >= 0 && used >= 0 ? Math.max(0, limit - used) : null;
}

/** The budget line for `{ mine, total }`: the smaller remainder of both, or null when either is malformed. */
export function budgetText(budget) {
  const mine = remaining(budget?.mine);
  const total = remaining(budget?.total);
  if (mine === null || total === null) return null;
  const left = Math.min(mine, total);
  return `You can add ${left} more ${left === 1 ? "track" : "tracks"} today.`;
}

const byName = (a, b) => String(a.name).localeCompare(String(b.name));
// Favorites first, then the caller's own, then by name.
const playlistOrder = (a, b) => Number(b.favorites === true) - Number(a.favorites === true) || Number(b.mine === true) - Number(a.mine === true) || byName(a, b);

/**
 * The Playlists page's sections: the caller's own (Favorites first), other
 * people's personal playlists by name, then the library's in its order.
 */
export function groupPlaylists(playlists) {
  const list = Array.isArray(playlists) ? playlists : [];
  return {
    mine: list.filter((playlist) => playlist.personal && playlist.mine).sort(playlistOrder),
    others: list.filter((playlist) => playlist.personal && !playlist.mine).sort(byName),
    library: list.filter((playlist) => !playlist.personal),
  };
}

/** The playlists the caller may add songs to, for the "Add to playlist" menu. */
export function editablePlaylists(playlists) {
  return (Array.isArray(playlists) ? playlists : []).filter((playlist) => playlist.editable === true).sort(playlistOrder);
}

/**
 * Index of the synced lyric line sung at `ms`: the last line that has begun,
 * or -1 before the first. `lines` are ordered by `start_ms`.
 */
export function lyricLineAt(lines, ms) {
  if (!Array.isArray(lines) || !Number.isFinite(ms)) return -1;
  let low = 0;
  let high = lines.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if ((lines[middle]?.start_ms ?? Infinity) <= ms) low = middle + 1;
    else high = middle;
  }
  return low - 1;
}

/** The Artists page's jump targets: name and index of every group that has artists. */
export function artistJumps(groups) {
  return (Array.isArray(groups) ? groups : []).flatMap((group, index) =>
    Array.isArray(group?.artists) && group.artists.length ? [{ name: String(group.name), index }] : []);
}

/**
 * The Genres page's list: the genres whose name contains `text` (trimmed,
 * ignoring case), by name, or with `order` "size" by songs, then albums,
 * largest first, then by name. Unknown counts count as 0.
 */
export function filterGenres(genres, text, order) {
  const needle = String(text ?? "").trim().toLocaleLowerCase();
  const byName = (a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
  const bySize = (a, b) => count(b.songs) - count(a.songs) || count(b.albums) - count(a.albums) || byName(a, b);
  return (Array.isArray(genres) ? genres : [])
    .filter((genre) => typeof genre?.name === "string" && genre.name.toLocaleLowerCase().includes(needle))
    .sort(order === "size" ? bySize : byName);
}

/** Shortest text, in characters, the search field asks suggestions for. */
export const SUGGEST_MIN_CHARS = 2;

/** The text to ask suggestions for: `text` trimmed, or null when it is shorter than `SUGGEST_MIN_CHARS`. */
export function suggestionQuery(text) {
  const trimmed = typeof text === "string" ? text.trim() : "";
  return [...trimmed].length >= SUGGEST_MIN_CHARS ? trimmed : null;
}

/**
 * The highlighted suggestion after one step `delta` (+1 down, -1 up) from
 * `active` in a list of `length`: wraps around, and -1 (nothing) steps to the
 * first or last item.
 */
export function suggestionStep(active, delta, length) {
  if (!(length > 0)) return -1;
  if (active < 0 || active >= length) return delta > 0 ? 0 : length - 1;
  return (active + delta + length) % length;
}

// DJ mode. The ranges match the bot's (docs/musicbot.md, "DJ mode").
export const DJ_EQ_MIN = -26;
export const DJ_EQ_MAX = 6;
export const DJ_TEMPO_RANGES = [8, 16];
export const DJ_TRANSITION_SECONDS = [2, 4, 8, 16, 30];
/** Continuous DJ controls send at most this often (about 30 a second). */
export const DJ_SEND_MS = 33;

/**
 * Song position in ms of a DJ deck: like interpolatePosition, with song time
 * running `tempo` percent faster than real time.
 */
export function deckPosition(positionMs, advancing, tempo, receivedAt, now, durationSeconds) {
  const rate = 1 + (Number.isFinite(tempo) ? tempo : 0) / 100;
  return interpolatePosition(positionMs, advancing, receivedAt, receivedAt + Math.max(0, now - receivedAt) * rate, durationSeconds);
}

/** The BPM of a song from the library at `tempo` percent, to one decimal, or null when unknown. */
export function deckBpm(bpm, tempo) {
  if (!(Number.isFinite(bpm) && bpm > 0)) return null;
  return Math.round(bpm * (1 + (Number.isFinite(tempo) ? tempo : 0) / 100) * 10) / 10;
}

/** The gains of decks A and B for a crossfader position (−1 A … 1 B) and curve, as the bot mixes. */
export function crossfadeGains(position, curve) {
  const travel = (clamp(Number(position) || 0, -1, 1) + 1) / 2;
  if (curve === "cut") return [Math.min(1, (1 - travel) / 0.04), Math.min(1, travel / 0.04)];
  return [Math.max(0, Math.cos(travel * Math.PI / 2)), Math.max(0, Math.sin(travel * Math.PI / 2))];
}

/** The deck the room hears most, as the bot decides: playing first, then louder, A on a tie; null without songs. */
export function leadDeck(dj) {
  if (!dj?.decks) return null;
  const crossfade = crossfadeGains(dj.crossfader, dj.curve);
  let lead = null;
  let best = null;
  dj.decks.forEach((deck, index) => {
    if (!deck.entry) return;
    const score = [deck.playing ? 1 : 0, (deck.volume / 100) ** 2 * crossfade[index]];
    if (best === null || score[0] > best[0] || (score[0] === best[0] && score[1] > best[1])) {
      best = score;
      lead = index;
    }
  });
  return lead;
}

/**
 * The crossfader position during a transition `sinceMs` after a state that
 * reported `position` at `elapsedMs` of `durationMs`, fading to deck `to`.
 */
export function transitionPosition(position, to, elapsedMs, durationMs, sinceMs) {
  const target = to === 0 ? -1 : 1;
  const left = durationMs - elapsedMs;
  if (!(left > 0)) return target;
  const share = clamp(Math.max(0, sinceMs) / left, 0, 1);
  return position + (target - position) * share;
}

const signed = (value, digits) => `${value > 0 ? "+" : value < 0 ? "−" : ""}${Math.abs(value).toFixed(digits)}`;

/** Spoken and shown value of an EQ band. */
export function decibelText(db) {
  return db <= DJ_EQ_MIN ? `${signed(DJ_EQ_MIN, 0)} dB, cut` : `${signed(Math.round(db * 2) / 2, 1)} dB`;
}

/** Spoken and shown value of the filter knob (−1 low-pass … 1 high-pass). */
export function filterText(value) {
  const percent = Math.round(Math.abs(value) * 100);
  if (percent === 0) return "Neutral";
  return `${value < 0 ? "Low-pass" : "High-pass"} ${percent} %`;
}

/** Spoken and shown tempo in percent. */
export function tempoText(percent) {
  return `${signed(Math.round(percent * 100) / 100, 2)} %`;
}

/** Spoken and shown crossfader position (−1 A … 1 B). */
export function crossfaderText(position) {
  const percent = Math.round(Math.abs(position) * 100);
  if (percent === 0) return "Centre";
  if (percent === 100) return position < 0 ? "Deck A only" : "Deck B only";
  return `${percent} % towards deck ${position < 0 ? "A" : "B"}`;
}

/**
 * Calls `send` (which may return a promise) with the latest arguments, one
 * call at a time and at most one per `ms`; arguments that arrive meanwhile
 * collapse into the newest. Requests of one control so arrive in order.
 * `cancel()` drops a waiting value; `idle()` is true with nothing waiting or running.
 */
export function paced(send, ms, now = () => Date.now()) {
  let pending = null;
  let busy = false;
  let last = -Infinity;
  let timer = null;
  async function pump() {
    if (busy || pending === null || timer !== null) return;
    const wait = last + ms - now();
    if (wait > 0) {
      timer = setTimeout(() => {
        timer = null;
        pump();
      }, wait);
      return;
    }
    const args = pending;
    pending = null;
    busy = true;
    last = now();
    try {
      await send(...args);
    } finally {
      busy = false;
      pump();
    }
  }
  const call = (...args) => {
    pending = args;
    pump();
  };
  call.cancel = () => {
    pending = null;
    clearTimeout(timer);
    timer = null;
  };
  call.idle = () => !busy && pending === null;
  return call;
}

/** Beat loop lengths in beats, as the DJ controls offer them. */
export const DJ_BEAT_LOOPS = [0.25, 0.5, 1, 2, 4, 8, 16, 32];

/** A beat count as a label: "1/4 beat", "1 beat", "8 beats". */
export function beatsText(beats) {
  const text = beats === 0.25 ? "1/4" : beats === 0.5 ? "1/2" : String(beats);
  return `${text} ${beats > 1 ? "beats" : "beat"}`;
}

// Web MIDI. A mapping binds DJ controls to the controls of one MIDI device.

/** Every DJ control MIDI can drive: `id`, `label`, `kind` ("range" or "button") and, for ranges, `center`. */
export function djControls() {
  const controls = [];
  for (const [deck, letter] of [[0, "A"], [1, "B"]]) {
    controls.push(
      { id: `${deck}.play`, label: `Play or pause deck ${letter}`, kind: "button" },
      { id: `${deck}.cue`, label: `Go to the cue point of deck ${letter}`, kind: "button" },
      { id: `${deck}.setCue`, label: `Set the cue point of deck ${letter}`, kind: "button" },
      { id: `${deck}.loadNext`, label: `Load deck ${letter}'s next song`, kind: "button" },
      { id: `${deck}.eject`, label: `Eject deck ${letter}`, kind: "button" },
      { id: `${deck}.eq.high`, label: `High EQ of deck ${letter}`, kind: "range", center: true },
      { id: `${deck}.eq.mid`, label: `Mid EQ of deck ${letter}`, kind: "range", center: true },
      { id: `${deck}.eq.low`, label: `Low EQ of deck ${letter}`, kind: "range", center: true },
      { id: `${deck}.filter`, label: `Filter of deck ${letter}`, kind: "range", center: true },
      { id: `${deck}.volume`, label: `Volume of deck ${letter}`, kind: "range", center: false },
      { id: `${deck}.tempo`, label: `Tempo of deck ${letter}`, kind: "range", center: true },
      { id: `${deck}.range`, label: `Tempo range of deck ${letter}`, kind: "button" },
      { id: `${deck}.sync`, label: `Sync deck ${letter}`, kind: "button" },
      { id: `${deck}.loopIn`, label: `Loop in on deck ${letter}`, kind: "button" },
      { id: `${deck}.loopOut`, label: `Loop out on deck ${letter}`, kind: "button" },
      { id: `${deck}.loopToggle`, label: `Loop on or off on deck ${letter}`, kind: "button" },
      ...DJ_BEAT_LOOPS.map((beats) => ({ id: `${deck}.beatLoop.${beats}`, label: `${beatsText(beats)} loop on deck ${letter}`, kind: "button" })),
      { id: `${deck}.loopHalve`, label: `Halve the loop of deck ${letter}`, kind: "button" },
      { id: `${deck}.loopDouble`, label: `Double the loop of deck ${letter}`, kind: "button" },
      { id: `${deck}.loopBack`, label: `Move the loop of deck ${letter} back`, kind: "button" },
      { id: `${deck}.loopForward`, label: `Move the loop of deck ${letter} forward`, kind: "button" },
      { id: `${deck}.quantize`, label: `Quantize on deck ${letter}`, kind: "button" },
    );
  }
  controls.push(
    { id: "crossfader", label: "Crossfader", kind: "range", center: true },
    { id: "curve", label: "Crossfader curve", kind: "button" },
    { id: "transition", label: "Fade to the other deck", kind: "button" },
    { id: "autoLoad", label: "Auto load", kind: "button" },
  );
  return controls;
}

/**
 * A MIDI message as `{ type: "cc" | "note", channel, number, value }`, with
 * note-off and note-on at velocity 0 as value 0; null for other messages.
 */
export function parseMidi(data) {
  if (!data || data.length < 3) return null;
  const status = data[0] & 0xf0;
  const message = { channel: data[0] & 0x0f, number: data[1] & 0x7f, value: data[2] & 0x7f };
  if (status === 0xb0) return { type: "cc", ...message };
  if (status === 0x90) return { type: "note", ...message };
  if (status === 0x80) return { type: "note", ...message, value: 0 };
  return null;
}

/**
 * The binding learned for `control` from the messages a learn window
 * collected, or null when none fits: ranges take a control change (14-bit
 * "fine" when the same channel also sent its LSB, controller number + 32),
 * buttons a note or a control change.
 */
export function learnBinding(control, messages) {
  const first = messages.find((message) => message && (message.type === "cc" || control.kind === "button"));
  if (!first) return null;
  const binding = { type: first.type, channel: first.channel, number: first.number, mode: "absolute" };
  if (control.kind === "range" && first.number < 32
    && messages.some((message) => message?.type === "cc" && message.channel === first.channel && message.number === first.number + 32)) {
    binding.mode = "fine";
  }
  return binding;
}

/** The step of a relative encoder value: 1…63 up, 65…127 down (two's complement), 64 none. */
export function relativeDelta(value) {
  return value === 64 ? 0 : value < 64 ? value : value - 128;
}

/** A button press: note-on, or a control change at 64 or more. Releases send nothing. */
export function midiPressed(message) {
  return message.value >= (message.type === "note" ? 1 : 64);
}

/** The fraction 0…1 of a range control from a 7-bit value, or from 14 bits with `lsb`. */
export function midiFraction(value, lsb = null) {
  return lsb === null ? clamp(value / 127, 0, 1) : clamp(((value << 7) | lsb) / 16383, 0, 1);
}

/**
 * The value of DJ control `id` for a fraction of its travel. Controls with a
 * middle rest there (0 dB, neutral, centre, 0 %) across the two middle steps
 * of a 7-bit knob. `range` is the deck's tempo range in percent.
 */
export function controlValue(id, fraction, range = 8) {
  let f = clamp(fraction, 0, 1);
  if (!id.endsWith("volume") && Math.abs(f - 0.5) < 0.75 / 127) f = 0.5;
  if (id.endsWith("volume")) return Math.round(f * 100);
  if (id.includes(".eq.")) return f < 0.5 ? DJ_EQ_MIN * (1 - f * 2) : DJ_EQ_MAX * (f - 0.5) * 2;
  if (id.endsWith("tempo")) return (f * 2 - 1) * range;
  return f * 2 - 1;
}

/** The fraction of travel of DJ control `id` at `value`: the inverse of controlValue. */
export function controlFraction(id, value, range = 8) {
  if (id.endsWith("volume")) return clamp(value / 100, 0, 1);
  if (id.includes(".eq.")) return clamp(value < 0 ? 0.5 - value / DJ_EQ_MIN / 2 : 0.5 + value / DJ_EQ_MAX / 2, 0, 1);
  if (id.endsWith("tempo")) return clamp((value / range + 1) / 2, 0, 1);
  return clamp((value + 1) / 2, 0, 1);
}

/** localStorage key of the mapping for the MIDI device named `name`. */
export function midiStorageKey(name) {
  return `nuschl-musicbot:midi:${String(name).slice(0, 200)}`;
}

const MIDI_TYPES = new Set(["cc", "note"]);
const MIDI_MODES = new Set(["absolute", "fine", "relative"]);
const isByte = (value, max) => Number.isInteger(value) && value >= 0 && value <= max;

/** A stored mapping (JSON) read back as `{ control: binding }`: unknown controls and malformed bindings drop out. */
export function parseMapping(text, controls = djControls()) {
  let stored;
  try {
    stored = typeof text === "string" && text.length <= 65536 ? JSON.parse(text) : null;
  } catch {
    return {};
  }
  const mapping = {};
  if (stored?.version !== 1 || typeof stored.controls !== "object" || stored.controls === null) return mapping;
  for (const control of controls) {
    const binding = stored.controls[control.id];
    if (!binding || !MIDI_TYPES.has(binding.type) || !isByte(binding.channel, 15) || !isByte(binding.number, 127)) continue;
    const mode = MIDI_MODES.has(binding.mode) ? binding.mode : "absolute";
    if (control.kind === "range" && binding.type !== "cc") continue;
    if (mode === "fine" && binding.number >= 32) continue;
    mapping[control.id] = { type: binding.type, channel: binding.channel, number: binding.number, mode: control.kind === "button" ? "absolute" : mode };
  }
  return mapping;
}

/** The JSON a mapping is stored as. */
export function serializeMapping(mapping) {
  return JSON.stringify({ version: 1, controls: mapping });
}

/**
 * The controls `message` drives under `mapping`: `{ id, binding, lsb }`,
 * where `lsb` marks the low half of a 14-bit control.
 */
export function boundControls(mapping, message) {
  const found = [];
  for (const [id, binding] of Object.entries(mapping)) {
    if (binding.type !== message.type || binding.channel !== message.channel) continue;
    if (binding.number === message.number) found.push({ id, binding, lsb: false });
    else if (binding.mode === "fine" && binding.number + 32 === message.number) found.push({ id, binding, lsb: true });
  }
  return found;
}

/** How a binding reads in the mapping table, such as "CC 7, channel 1". */
export function bindingText(binding) {
  if (!binding) return "—";
  const what = binding.type === "note" ? `Note ${binding.number}` : `CC ${binding.number}`;
  return `${what}, channel ${binding.channel + 1}${binding.mode === "fine" ? ", 14-bit" : ""}`;
}

// Keyboard shortcuts for the DJ view. A shortcut is a physical key
// (`KeyboardEvent.code`, the same on every layout) with or without Shift.

/** Escape cancels and closes, Tab moves the focus; shortcuts leave both to the page. */
const RESERVED_KEYS = new Set(["Escape", "Tab"]);
const MODIFIER_KEYS = new Set([
  "ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight",
  "OSLeft", "OSRight", "CapsLock", "Fn", "FnLock", "NumLock", "ScrollLock", "ContextMenu",
]);
const KEY_CODE = /^[A-Z][A-Za-z0-9]{0,31}$/;

/** True for a `KeyboardEvent.code` a shortcut can use. */
function bindableCode(code) {
  return typeof code === "string" && KEY_CODE.test(code) && !RESERVED_KEYS.has(code) && !MODIFIER_KEYS.has(code);
}

/**
 * The continuous controls keys move: `step` per press and `rate` per second
 * while the key stays down, rounded to `digits` decimals, within `min`…`max`
 * (tempo: ± the deck's tempo range).
 */
export const DJ_KEY_GLIDES = {
  volume: { step: 5, rate: 50, min: 0, max: 100, digits: 0 },
  tempo: { step: 0.1, rate: 1, min: null, max: null, digits: 2 },
  filter: { step: 0.05, rate: 0.5, min: -1, max: 1, digits: 2 },
  crossfader: { step: 0.05, rate: 1, min: -1, max: 1, digits: 2 },
};
/** A key held this long starts gliding; a quick press moves one step. */
export const DJ_KEY_GLIDE_DELAY_MS = 250;

/**
 * The value of continuous `control` after its key has been down for `heldMs`
 * from `start`: one step at once, then `rate` per second after the glide delay.
 * `direction` is −1 or 1; `range` bounds the tempo in percent.
 */
export function glideValue(control, start, direction, heldMs, range = 8) {
  const glide = DJ_KEY_GLIDES[control];
  const min = control === "tempo" ? -range : glide.min;
  const max = control === "tempo" ? range : glide.max;
  const travel = glide.step + glide.rate * Math.max(0, (Number(heldMs) || 0) - DJ_KEY_GLIDE_DELAY_MS) / 1000;
  const scale = 10 ** glide.digits;
  return clamp(Math.round(((Number(start) || 0) + direction * travel) * scale) / scale, min, max);
}

/** The value of continuous `control` after one key press from `value`. */
export function keyStep(control, value, direction, range = 8) {
  return glideValue(control, value, direction, 0, range);
}

/** The EQ value a kill key sets: the kill at −26 dB, or back to `previous` (0 when unknown) from a kill. */
export function eqKillTarget(current, previous) {
  if (current > DJ_EQ_MIN) return DJ_EQ_MIN;
  return Number.isFinite(previous) && previous > DJ_EQ_MIN ? previous : 0;
}

/**
 * Every DJ action a key can trigger, in the order the shortcuts panel lists
 * them: `id`, `label`, `group` ("Deck A", "Mixer", "Deck B"), `deck` (0, 1
 * or null), `name`, the default `key` and, for continuous controls, `glide`
 * (`{ control, direction }`); EQ kills carry `band` and beat loops `beats`.
 */
export function djKeyActions() {
  const layouts = [
    [0, {
      play: "KeyD", cue: "KeyF", seekBack: "KeyA", seekForward: "KeyQ", tempoDown: "KeyS", tempoUp: "KeyW",
      volumeDown: "KeyC", volumeUp: "KeyE", filterLeft: "KeyZ", filterRight: "KeyX", sync: "KeyR", loadNext: "KeyT",
      headphones: "KeyV", low: "Digit1", mid: "Digit2", high: "Digit3", loop4: "Digit4", loop8: "Digit5",
    }],
    [1, {
      play: "KeyL", cue: "Semicolon", seekBack: "KeyJ", seekForward: "KeyU", tempoDown: "KeyK", tempoUp: "KeyI",
      volumeDown: "Period", volumeUp: "KeyO", filterLeft: "KeyM", filterRight: "Comma", sync: "KeyP", loadNext: "KeyY",
      headphones: "Slash", low: "Digit8", mid: "Digit9", high: "Digit0", loop4: "Minus", loop8: "Equal",
    }],
  ];
  const actions = [];
  for (const [deck, k] of layouts) {
    const group = deck === 0 ? "Deck A" : "Deck B";
    const add = (name, label, code, shift, extra = {}) => actions.push({ id: `${deck}.${name}`, label, group, deck, name, key: { code, shift }, ...extra });
    add("play", "Play or pause", k.play, false);
    add("cue", "Go to the cue point", k.cue, false);
    add("setCue", "Set the cue point", k.cue, true);
    add("seekBack", "Seek back 4 s", k.seekBack, false);
    add("seekForward", "Seek forward 4 s", k.seekForward, false);
    add("tempoDown", "Tempo down", k.tempoDown, false, { glide: { control: "tempo", direction: -1 } });
    add("tempoUp", "Tempo up", k.tempoUp, false, { glide: { control: "tempo", direction: 1 } });
    add("volumeDown", "Volume down", k.volumeDown, false, { glide: { control: "volume", direction: -1 } });
    add("volumeUp", "Volume up", k.volumeUp, false, { glide: { control: "volume", direction: 1 } });
    add("filterLeft", "Filter towards low-pass", k.filterLeft, false, { glide: { control: "filter", direction: -1 } });
    add("filterRight", "Filter towards high-pass", k.filterRight, false, { glide: { control: "filter", direction: 1 } });
    add("kill.low", "Cut or restore the low EQ", k.low, false, { band: "low" });
    add("kill.mid", "Cut or restore the mid EQ", k.mid, false, { band: "mid" });
    add("kill.high", "Cut or restore the high EQ", k.high, false, { band: "high" });
    add("sync", "Sync to the other deck", k.sync, false);
    add("loadNext", "Load the deck's next song", k.loadNext, false);
    add("headphones", "Headphone cue on or off", k.headphones, false);
    add("loopIn", "Loop in", k.filterLeft, true);
    add("loopOut", "Loop out", k.filterRight, true);
    add("loopToggle", "Loop on or off", k.play, true);
    add("beatLoop.4", "4 beat loop", k.loop4, false, { beats: 4 });
    add("beatLoop.8", "8 beat loop", k.loop8, false, { beats: 8 });
    add("loopHalve", "Halve the loop", k.tempoDown, true);
    add("loopDouble", "Double the loop", k.tempoUp, true);
    add("loopBack", "Move the loop back", k.seekBack, true);
    add("loopForward", "Move the loop forward", k.seekForward, true);
    if (deck === 0) {
      const mixer = (id, label, code, shift, extra = {}) => actions.push({ id, label, group: "Mixer", deck: null, name: id, key: { code, shift }, ...extra });
      mixer("crossfaderLeft", "Crossfader towards A", "KeyG", false, { glide: { control: "crossfader", direction: -1 } });
      mixer("crossfaderRight", "Crossfader towards B", "KeyH", false, { glide: { control: "crossfader", direction: 1 } });
      mixer("crossfaderA", "Crossfader to deck A", "KeyG", true);
      mixer("crossfaderB", "Crossfader to deck B", "KeyH", true);
      mixer("transition", "Fade to the other deck", "KeyB", false);
      mixer("autoLoad", "Auto load on or off", "KeyN", false);
      mixer("help", "Show the keyboard shortcuts", "Slash", true);
    }
  }
  return actions;
}

/** The default bindings: `{ actionId: { code, shift } }`. */
export function defaultKeyBindings(actions = djKeyActions()) {
  return Object.fromEntries(actions.map((action) => [action.id, { ...action.key }]));
}

const KEY_NAMES = {
  Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/", Backslash: "\\", BracketLeft: "[", BracketRight: "]",
  Minus: "-", Equal: "=", Backquote: "`", IntlBackslash: "<", ArrowLeft: "←", ArrowRight: "→", ArrowUp: "↑", ArrowDown: "↓",
};

/**
 * How a binding reads, such as "Shift + ;". `layout` (a map from code to the
 * character, as `navigator.keyboard.getLayoutMap()` gives) names keys as this
 * keyboard prints them; the US layout names the rest.
 */
export function keyLabel(binding, layout = null) {
  if (!binding) return "—";
  const { code } = binding;
  const printed = typeof layout?.get === "function" ? layout.get(code) : undefined;
  let name;
  if (typeof printed === "string" && printed.trim() !== "" && [...printed].length === 1) name = printed.toLocaleUpperCase();
  else if (/^Key[A-Z]$/.test(code)) name = code.slice(3);
  else if (/^Digit\d$/.test(code)) name = code.slice(5);
  else if (/^Numpad\d$/.test(code)) name = `Num ${code.slice(6)}`;
  else name = KEY_NAMES[code] ?? code.replace(/([a-z0-9])([A-Z])/g, "$1 $2");
  return binding.shift ? `Shift + ${name}` : name;
}

/** The shortcut key of a keyboard event as `{ code, shift }`, or null for presses with Ctrl, Alt or Meta, modifiers alone, Escape, Tab and composition. */
export function keyFromEvent(event) {
  if (!event || event.ctrlKey || event.altKey || event.metaKey || event.isComposing || !bindableCode(event.code)) return null;
  return { code: event.code, shift: event.shiftKey === true };
}

/** True when `key` (from keyFromEvent) is `binding`. */
export function keyMatches(binding, key) {
  return !!binding && !!key && binding.code === key.code && binding.shift === key.shift;
}

const keyId = (binding) => `${binding.shift ? "Shift+" : ""}${binding.code}`;

/** Keys bound to more than one action: `[{ key, actions: [actionId, …] }]`. */
export function findConflicts(bindings) {
  const byKey = new Map();
  for (const [id, binding] of Object.entries(bindings)) {
    if (!binding) continue;
    const found = byKey.get(keyId(binding));
    if (found) found.actions.push(id);
    else byKey.set(keyId(binding), { key: { code: binding.code, shift: binding.shift }, actions: [id] });
  }
  return [...byKey.values()].filter((entry) => entry.actions.length > 1);
}

/** `bindings` with `key` on action `id`; `moved` lists the actions that held `key` before and are unbound now. */
export function bindKey(bindings, id, key) {
  const next = {};
  const moved = [];
  for (const [other, binding] of Object.entries(bindings)) {
    if (other !== id && keyMatches(binding, key)) {
      next[other] = null;
      moved.push(other);
    } else {
      next[other] = binding;
    }
  }
  next[id] = { code: key.code, shift: key.shift };
  return { bindings: next, moved };
}

const validKey = (value) => value !== null && typeof value === "object" && bindableCode(value.code) && typeof value.shift === "boolean";

/**
 * Stored bindings (JSON) read back as `{ actionId: { code, shift } | null }`
 * for every action: stored keys first, then each remaining action's default
 * while its key is free. Unknown actions, malformed keys and repeats of a key
 * drop out.
 */
export function parseKeyBindings(text, actions = djKeyActions()) {
  let stored = null;
  try {
    stored = typeof text === "string" && text.length <= 16384 ? JSON.parse(text) : null;
  } catch {
    stored = null;
  }
  const keys = stored?.version === 1 && stored.keys !== null && typeof stored.keys === "object" && !Array.isArray(stored.keys) ? stored.keys : {};
  const bindings = {};
  const used = new Set();
  for (const action of actions) {
    if (!Object.hasOwn(keys, action.id)) continue;
    const value = keys[action.id];
    if (value === null) bindings[action.id] = null;
    else if (validKey(value) && !used.has(keyId(value))) {
      bindings[action.id] = { code: value.code, shift: value.shift };
      used.add(keyId(value));
    }
  }
  for (const action of actions) {
    if (Object.hasOwn(bindings, action.id)) continue;
    const free = !used.has(keyId(action.key));
    bindings[action.id] = free ? { ...action.key } : null;
    if (free) used.add(keyId(action.key));
  }
  return bindings;
}

/** The JSON bindings are stored as. */
export function serializeKeyBindings(bindings) {
  return JSON.stringify({ version: 1, keys: bindings });
}

const KEY_INPUT_TYPES = new Set(["range", "checkbox", "radio", "button", "submit", "reset", "color", "file", "image"]);

/** True when a keyboard event target takes typed text: text-like inputs, textareas, selects and editable content. */
export function typingTarget(target) {
  if (!target || typeof target !== "object") return false;
  if (target.isContentEditable === true) return true;
  const tag = String(target.tagName ?? "").toUpperCase();
  if (tag === "TEXTAREA" || tag === "SELECT") return true;
  return tag === "INPUT" && !KEY_INPUT_TYPES.has(String(target.type || "text").toLowerCase());
}

// DJ waveforms: spectrum columns from `api/spectrum`. Column n covers song
// time [n * 10 ms, (n + 1) * 10 ms); a byte is a level from −72 dBFS (0) to
// 0 dBFS (255), linear in dB.

export const SPECTRUM_COLUMN_MS = 10;
/** Most columns one `api/spectrum` request returns. */
export const SPECTRUM_MAX_COLUMNS = 2000;
/** Half-widths of the waveform window in seconds, as the zoom buttons offer them. */
export const DJ_WAVE_ZOOMS = [3, 6, 12];

/** Level in dB of a spectrum byte. */
export function levelDb(byte) {
  return (byte * 72) / 255 - 72;
}

/** Bar height 0..1 of a spectrum byte: −54 dBFS and below draw nothing, 0 dBFS the full height. */
export function levelHeight(byte) {
  return clamp((levelDb(byte) + 54) / 54, 0, 1);
}

/** Visible columns [from, to) of a window ±`zoomS` seconds around `positionMs`; `from` may be negative. */
export function waveWindow(positionMs, zoomS) {
  const centre = positionMs / SPECTRUM_COLUMN_MS;
  const half = (zoomS * 1000) / SPECTRUM_COLUMN_MS;
  return { from: Math.floor(centre - half), to: Math.ceil(centre + half) };
}

/** Horizontal position of song time `ms` on a canvas `width` wide that centres `positionMs` and spans ±`zoomS` seconds. */
export function timeX(ms, positionMs, zoomS, width) {
  return width / 2 + ((ms - positionMs) / (zoomS * 2000)) * width;
}

/** Song time at horizontal position `x`; the inverse of timeX. */
export function xTime(x, positionMs, zoomS, width) {
  return positionMs + ((x - width / 2) / width) * zoomS * 2000;
}

/**
 * Column ranges to request for the `wanted` range: the parts outside every
 * range in `cached` (sorted or not, each `{ from, to }`), below `computed`
 * (columns the bot has so far; null when unknown), from column 0, split into
 * requests of at most `maxCount` columns.
 */
export function rangesToFetch(cached, wanted, computed, maxCount = SPECTRUM_MAX_COLUMNS) {
  const end = computed == null ? wanted.to : Math.min(wanted.to, computed);
  const sorted = [...cached].sort((a, b) => a.from - b.from);
  const out = [];
  let at = Math.max(0, wanted.from);
  const take = (to) => {
    for (; at < to; at = Math.min(to, at + maxCount)) out.push({ from: at, to: Math.min(to, at + maxCount) });
  };
  for (const range of sorted) {
    if (at >= end) break;
    if (range.to <= at) continue;
    if (range.from > at) take(Math.min(range.from, end));
    at = Math.max(at, range.to);
  }
  take(end);
  return out;
}

/** Index of the range in `ranges` (sorted by `from`, disjoint) that holds `column`, or -1. */
export function findRange(ranges, column) {
  let low = 0;
  let high = ranges.length - 1;
  while (low <= high) {
    const mid = (low + high) >> 1;
    if (column < ranges[mid].from) high = mid - 1;
    else if (column >= ranges[mid].to) low = mid + 1;
    else return mid;
  }
  return -1;
}

/** Copy of `ranges` (sorted by `from`, disjoint) with `range` added in order, replacing the ranges it overlaps. */
export function insertRange(ranges, range) {
  const out = ranges.filter((item) => item.to <= range.from || item.from >= range.to);
  const index = out.findIndex((item) => item.from > range.from);
  out.splice(index < 0 ? out.length : index, 0, range);
  return out;
}

/** Distance in columns from `column` to the range `{ from, to }`; 0 inside it. */
const rangeDistance = (range, column) => (column < range.from ? range.from - column : column >= range.to ? column - range.to + 1 : 0);

/**
 * Copy of `ranges` holding the ones within `keep` columns of `column`, and at
 * most `cap` columns in all: the nearest ranges stay.
 */
export function pruneRanges(ranges, column, keep, cap) {
  const near = ranges.filter((range) => rangeDistance(range, column) <= keep);
  const byDistance = [...near].sort((a, b) => rangeDistance(a, column) - rangeDistance(b, column));
  const kept = new Set();
  let total = 0;
  for (const range of byDistance) {
    total += range.to - range.from;
    if (total > cap && kept.size) break;
    kept.add(range);
  }
  return near.filter((range) => kept.has(range));
}

/** `[r, g, b]` of a CSS colour in `#rgb`, `#rrggbb`, `rgb()` or `rgba()` form, or null. */
export function parseColour(text) {
  const value = String(text ?? "").trim().toLowerCase();
  let match = /^#([0-9a-f]{3})$/.exec(value);
  if (match) return [...match[1]].map((digit) => parseInt(digit + digit, 16));
  match = /^#([0-9a-f]{6})$/.exec(value);
  if (match) return [0, 2, 4].map((at) => parseInt(match[1].slice(at, at + 2), 16));
  match = /^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)/.exec(value);
  if (match) return match.slice(1, 4).map((part) => clamp(Math.round(Number(part)), 0, 255));
  return null;
}

/** Heat-map colour `[r, g, b]` of a spectrum byte between evenly spread colour `stops` (quiet first). */
export function heatColour(byte, stops) {
  const at = (clamp(byte, 0, 255) / 255) * (stops.length - 1);
  const index = Math.min(Math.floor(at), stops.length - 2);
  const share = at - index;
  const [a, b] = [stops[index], stops[index + 1] ?? stops[index]];
  return a.map((channel, i) => Math.round(channel + (b[i] - channel) * share));
}

/**
 * Beat-grid ticks between `fromMs` and `toMs` for `grid` (`{ bpm, first_beat_ms }`
 * or null), at most `maxTicks`: every beat, or every 4th, 16th … beat when
 * they would be more. `bar` marks every 4th beat from the first beat.
 */
export function gridTicks(grid, fromMs, toMs, maxTicks) {
  if (!grid || !(grid.bpm > 0) || !Number.isFinite(grid.first_beat_ms) || !(toMs > fromMs)) return [];
  const beat = 60000 / grid.bpm;
  let step = 1;
  while ((toMs - fromMs) / (beat * step) > maxTicks) step *= 4;
  const span = beat * step;
  const ticks = [];
  for (let k = Math.ceil((fromMs - grid.first_beat_ms) / span); grid.first_beat_ms + k * span <= toMs; k++) {
    ticks.push({ ms: grid.first_beat_ms + k * span, bar: step > 1 || ((k % 4) + 4) % 4 === 0 });
  }
  return ticks;
}
