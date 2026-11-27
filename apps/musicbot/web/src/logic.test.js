// SPDX-License-Identifier: EUPL-1.2
import { test, mock } from "node:test";
import assert from "node:assert/strict";
import {
  formatDuration, totalDuration, interpolatePosition, pointerFraction, seekKeyTarget, seekSettled,
  gapIndex, moveTarget, applyMove, stepTarget, parseRoute, buildRoute, routeForLibrary, debounce,
  queueStorageKey, selectedQueue, encodeDragItem, decodeDragItem, handlesSpace,
  mergeHistory, libraryForNamespace, routeForNamespace, browseItemIsCurrent, similarityPercent, sonicPathStart,
  shareRoute, shareRoot, shareUrl, addressRoute, adoptSharedRoute,
  isImportId, releaseTypeLabel, requestStatusText, requestProgress, budgetText, coverRetryDelay, COVER_RETRIES,
  groupPlaylists, editablePlaylists, lyricLineAt, artistJumps, suggestionQuery, suggestionStep, filterGenres,
  deckPosition, deckBpm, crossfadeGains, leadDeck, transitionPosition, decibelText, filterText, tempoText,
  crossfaderText, paced, djControls, parseMidi, learnBinding, relativeDelta, midiPressed, midiFraction,
  controlValue, controlFraction, midiStorageKey, parseMapping, serializeMapping, boundControls, bindingText,
  DJ_BEAT_LOOPS, beatsText, DJ_KEY_GLIDES, DJ_KEY_GLIDE_DELAY_MS, glideValue, keyStep, eqKillTarget, djKeyActions,
  defaultKeyBindings, keyLabel, keyFromEvent, keyMatches, findConflicts, bindKey, parseKeyBindings,
  serializeKeyBindings, typingTarget,
  SPECTRUM_MAX_COLUMNS, levelDb, levelHeight, waveWindow, timeX, xTime, rangesToFetch, findRange, insertRange,
  pruneRanges, parseColour, heatColour, gridTicks,
} from "./logic.js";

test("formatDuration", () => {
  assert.equal(formatDuration(0), "0:00");
  assert.equal(formatDuration(7.9), "0:07");
  assert.equal(formatDuration(187), "3:07");
  assert.equal(formatDuration(3599), "59:59");
  assert.equal(formatDuration(3600), "1:00:00");
  assert.equal(formatDuration(3723), "1:02:03");
  assert.equal(formatDuration(-5), "0:00");
  assert.equal(formatDuration(null), "");
  assert.equal(formatDuration(NaN), "");
  assert.equal(formatDuration(Infinity), "");
});

test("totalDuration skips unknown durations", () => {
  assert.equal(totalDuration([{ duration: 10 }, { duration: null }, { duration: 5 }]), 15);
  assert.equal(totalDuration([]), 0);
});

test("interpolatePosition advances, caps and never goes negative", () => {
  assert.equal(interpolatePosition(1000, true, 500, 2500, 60), 3000);
  assert.equal(interpolatePosition(1000, false, 500, 2500, 60), 1000);
  assert.equal(interpolatePosition(59000, true, 0, 5000, 60), 60000);
  assert.equal(interpolatePosition(1000, true, 0, 5000, null), 6000);
  // A clock that runs backwards relative to the snapshot does not rewind.
  assert.equal(interpolatePosition(1000, true, 5000, 4000, 60), 1000);
  assert.equal(interpolatePosition(-20, false, 0, 0, 60), 0);
});

test("pointerFraction clamps to the bar", () => {
  assert.equal(pointerFraction(50, 0, 200), 0.25);
  assert.equal(pointerFraction(-10, 0, 200), 0);
  assert.equal(pointerFraction(300, 0, 200), 1);
  assert.equal(pointerFraction(10, 0, 0), 0);
});

test("seekKeyTarget", () => {
  assert.equal(seekKeyTarget("ArrowRight", 10000, 60000), 15000);
  assert.equal(seekKeyTarget("ArrowLeft", 3000, 60000), 0);
  assert.equal(seekKeyTarget("ArrowRight", 58000, 60000), 60000);
  assert.equal(seekKeyTarget("Home", 30000, 60000), 0);
  assert.equal(seekKeyTarget("End", 30000, 60000), 60000);
  assert.equal(seekKeyTarget("PageUp", 0, 60000), 30000);
  assert.equal(seekKeyTarget("a", 0, 60000), null);
});

test("seekSettled", () => {
  const current = { entry: { id: 4 } };
  assert.equal(seekSettled(30000, 4, current, 10000), false);
  assert.equal(seekSettled(30000, 4, current, 30400), true);
  assert.equal(seekSettled(30000, 4, { entry: { id: 5 } }, 0), true);
  assert.equal(seekSettled(30000, 4, null, 0), true);
});

test("gapIndex picks the half of the row", () => {
  assert.equal(gapIndex(3, 0, 40), 3);
  assert.equal(gapIndex(3, 19.9, 40), 3);
  assert.equal(gapIndex(3, 20, 40), 4);
  assert.equal(gapIndex(3, 40, 40), 4);
});

test("moveTarget counts the index without the dragged entry", () => {
  // [a b c d e], dragging b (index 1)
  assert.equal(moveTarget(1, 0, 5), 0); // before a
  assert.equal(moveTarget(1, 1, 5), null); // just before itself
  assert.equal(moveTarget(1, 2, 5), null); // just after itself
  assert.equal(moveTarget(1, 3, 5), 2); // between c and d: [a c b d e]
  assert.equal(moveTarget(1, 5, 5), 4); // after e
  assert.equal(moveTarget(1, 99, 5), 4); // clamped
  assert.equal(moveTarget(1, -3, 5), 0); // clamped
  assert.equal(moveTarget(4, 0, 5), 0);
  assert.equal(moveTarget(0, 1, 1), null);
  assert.equal(moveTarget(5, 0, 5), null); // not in the list
  const list = ["a", "b", "c", "d", "e"];
  assert.deepEqual(applyMove(list, 1, moveTarget(1, 3, 5)), ["a", "c", "b", "d", "e"]);
  assert.deepEqual(applyMove(list, 1, moveTarget(1, 5, 5)), ["a", "c", "d", "e", "b"]);
  assert.deepEqual(applyMove(list, 3, moveTarget(3, 0, 5)), ["d", "a", "b", "c", "e"]);
});

test("applyMove clamps and leaves the input alone", () => {
  const list = ["a", "b", "c"];
  assert.deepEqual(applyMove(list, 0, 10), ["b", "c", "a"]);
  assert.deepEqual(applyMove(list, 2, -1), ["c", "a", "b"]);
  assert.deepEqual(applyMove(list, 7, 0), ["a", "b", "c"]);
  assert.deepEqual(list, ["a", "b", "c"]);
});

test("stepTarget stops at the ends", () => {
  assert.equal(stepTarget(0, -1, 3), null);
  assert.equal(stepTarget(0, 1, 3), 1);
  assert.equal(stepTarget(2, 1, 3), null);
  assert.deepEqual(applyMove(["a", "b", "c"], 0, stepTarget(0, 1, 3)), ["b", "a", "c"]);
  assert.deepEqual(applyMove(["a", "b", "c"], 2, stepTarget(2, -1, 3)), ["a", "c", "b"]);
});

test("routes round trip, including awkward ids", () => {
  const ids = ["plain", "a/b", "what?", "#hash", "with space", "50% & more", "ü/€?#=&+"];
  for (const view of ["album", "artist", "playlist", "song"]) {
    for (const id of ids) {
      const route = { view, library: 2, id };
      const hash = buildRoute(route);
      assert.equal(hash.indexOf("#", 1), -1, hash);
      assert.equal(hash.split("/").length, 4, hash);
      assert.deepEqual(parseRoute(hash), route);
    }
  }
  for (const id of ids) {
    const similar = { view: "similar", library: 2, id, namespace: "page:0" };
    const hash = buildRoute(similar);
    assert.equal(hash.indexOf("#", 1), -1, hash);
    assert.equal(hash.split("?")[0].split("/").length, 4, hash);
    assert.deepEqual(parseRoute(hash), similar);
    for (const to of ids) {
      const path = { view: "path", library: 1, from: id, to, namespace: "page:0" };
      const pathHash = buildRoute(path);
      assert.equal(pathHash.indexOf("#", 1), -1, pathHash);
      assert.equal(pathHash.split("?")[0].split("/").length, 5, pathHash);
      assert.deepEqual(parseRoute(pathHash), path);
    }
  }
  for (const route of [
    { view: "queue" },
    { view: "channels" },
    { view: "history" },
    { view: "dj" },
    { view: "albums", library: 1, list: "starred" },
    { view: "artists", library: 0 },
    { view: "playlists", library: 3 },
    { view: "genres", library: 2 },
    { view: "genre", library: 1, name: "Rock" },
    { view: "search", q: "a b/c?d#e&f=g+h" },
    { view: "search", q: "" },
    { view: "add", q: "a b/c?d#e&f=g+h" },
    { view: "add", q: "" },
    { view: "addAlbum", id: "0123456789abcdef" },
    { view: "requests" },
  ]) assert.deepEqual(parseRoute(buildRoute(route)), route);
});

test("genre routes keep odd names in the query", () => {
  const names = ["Rock; Pop", "R&B", "AC/DC Covers", "Drum + Bass", "50% & more", "a=b?c#d", "Música Popular Brasileira", "日本のロック", "x".repeat(200)];
  for (const name of names) {
    const route = { view: "genre", library: 2, name };
    const hash = buildRoute(route);
    assert.ok(hash.startsWith("#/genre?"), hash);
    assert.equal(hash.indexOf("#", 1), -1, hash);
    assert.equal(hash.indexOf("/", 2), -1, hash);
    assert.deepEqual(parseRoute(hash), route);
    const shared = shareRoute(route, ["k0", "k1", "k2"]);
    assert.deepEqual(parseRoute(shared), { ...route, sharedLibrary: "k2" });
  }
  assert.equal(buildRoute({ view: "genre", library: 0, name: "R&B / Soul" }), "#/genre?library=0&name=R%26B+%2F+Soul");
  assert.equal(buildRoute({ view: "genres", library: 1 }), "#/genres?library=1");
  assert.deepEqual(parseRoute("#/genre?library=1&name=Jazz+Fusion"), { view: "genre", library: 1, name: "Jazz Fusion" });
  assert.deepEqual(parseRoute("#/genre?library=1"), { view: "genres", library: 1 });
  assert.deepEqual(parseRoute("#/genre?library=1&name="), { view: "genres", library: 1 });
  assert.deepEqual(parseRoute("#/genres?library=x"), { view: "genres", library: 0 });
  assert.deepEqual(routeForLibrary({ view: "genre", library: 0, name: "Jazz" }, 1), { view: "genre", library: 1, name: "Jazz" });
  assert.deepEqual(routeForLibrary({ view: "genres", library: 0 }, 2), { view: "genres", library: 2 });
});

test("the genre list narrows by name and sorts by name or size", () => {
  const genres = [
    { name: "rock", albums: 2, songs: 30 },
    { name: "Ambient", albums: 5, songs: 30 },
    { name: "Électro", albums: null, songs: 80 },
    { name: "Jazz Rock", albums: 1, songs: null },
    { name: "Rockabilly", albums: 1, songs: 4 },
  ];
  const names = (list) => list.map((genre) => genre.name);
  assert.deepEqual(names(filterGenres(genres, "", "name")), ["Ambient", "Électro", "Jazz Rock", "rock", "Rockabilly"]);
  assert.deepEqual(names(filterGenres(genres, "", "size")), ["Électro", "Ambient", "rock", "Rockabilly", "Jazz Rock"]);
  assert.deepEqual(names(filterGenres(genres, "  ROCK ", "name")), ["Jazz Rock", "rock", "Rockabilly"]);
  assert.deepEqual(names(filterGenres(genres, "élec", "size")), ["Électro"]);
  assert.deepEqual(filterGenres(genres, "polka", "name"), []);
  assert.deepEqual(filterGenres(null, "", "name"), []);
  assert.equal(genres[0].name, "rock", "the input keeps its order");
});

test("import routes", () => {
  assert.equal(buildRoute({ view: "add", q: "Miles Davis" }), "#/add?q=Miles+Davis");
  assert.equal(buildRoute({ view: "add" }), "#/add?q=");
  assert.equal(buildRoute({ view: "addAlbum", id: "0123456789abcdef" }), "#/add/album/0123456789abcdef");
  assert.equal(buildRoute({ view: "requests" }), "#/requests");
  assert.deepEqual(parseRoute("#/add"), { view: "add", q: "" });
  assert.deepEqual(parseRoute("#/add?q=kind%20of%20blue"), { view: "add", q: "kind of blue" });
  assert.deepEqual(parseRoute("#/add/album/fedcba9876543210"), { view: "addAlbum", id: "fedcba9876543210" });
  assert.deepEqual(parseRoute("#/requests"), { view: "requests" });
  assert.deepEqual(parseRoute("#/requests?library_key=x"), { view: "requests" });
  // Only well-formed import ids reach the album page.
  for (const hash of ["#/add/album", "#/add/album/ABCDEF0123456789", "#/add/album/0123456789abcde", "#/add/album/0123456789abcdef0",
    "#/add/album/..%2F..%2Fapi", "#/add/album/0123456789abcdef/x", "#/add/song/0123456789abcdef", "#/add/album/%E0%A4%A"]) {
    assert.deepEqual(parseRoute(hash), hash.endsWith("%A") ? { view: "queue" } : { view: "add", q: "" }, hash);
  }
  // Import routes carry no library and stay unchanged across namespaces and libraries.
  for (const route of [{ view: "add", q: "x" }, { view: "addAlbum", id: "0123456789abcdef" }, { view: "requests" }]) {
    assert.deepEqual(routeForLibrary(route, 2), route);
    assert.deepEqual(routeForNamespace(route, 1, null), route);
    assert.equal(shareRoute(route, ["k"]), buildRoute(route));
  }
});

test("isImportId accepts exactly 16 lowercase hex characters", () => {
  assert.ok(isImportId("0123456789abcdef"));
  for (const id of ["", "0123456789ABCDEF", "0123456789abcde", "0123456789abcdefa", "0123456789abcdeg", null, 123, "../0123456789abc"]) {
    assert.equal(isImportId(id), false, String(id));
  }
});

test("releaseTypeLabel", () => {
  assert.equal(releaseTypeLabel("album"), "Album");
  assert.equal(releaseTypeLabel("ep"), "EP");
  assert.equal(releaseTypeLabel("single"), "Single");
  assert.equal(releaseTypeLabel("compilation"), "Compilation");
  assert.equal(releaseTypeLabel("toString"), "");
  assert.equal(releaseTypeLabel(undefined), "");
});

test("request status texts", () => {
  const request = (status, tracks_done = 0, tracks_failed = 0) => ({ status, tracks_total: 12, tracks_done, tracks_failed });
  assert.equal(requestStatusText(request("queued")), "Waiting to start");
  assert.equal(requestStatusText(request("downloading", 5)), "Adding · 5 of 12 tracks");
  assert.equal(requestStatusText(request("handed_over", 12)),
    "Added. Shows up in the library within a few minutes.");
  assert.equal(requestStatusText(request("handed_over", 9, 3)),
    "Added 9 of 12 tracks. They show up in the library within a few minutes.");
  assert.equal(requestStatusText(request("failed")), "Could not be added.");
  assert.equal(requestStatusText(request("bogus")), "");
  assert.equal(requestStatusText(null), "");
  // Malformed counts never produce negative or fractional numbers.
  assert.equal(requestStatusText({ status: "downloading", tracks_total: -1, tracks_done: 2.5 }), "Adding · 0 of 0 tracks");
  assert.equal(requestStatusText({ status: "handed_over", tracks_total: 2, tracks_failed: 5 }),
    "Added 0 of 2 tracks. They show up in the library within a few minutes.");
});

test("request progress shows only while adding", () => {
  assert.deepEqual(requestProgress({ status: "downloading", tracks_total: 10, tracks_done: 4 }), { value: 4, max: 10 });
  assert.deepEqual(requestProgress({ status: "downloading", tracks_total: 10, tracks_done: 40 }), { value: 10, max: 10 });
  assert.equal(requestProgress({ status: "downloading", tracks_total: 0, tracks_done: 0 }), null);
  for (const status of ["queued", "handed_over", "failed"]) assert.equal(requestProgress({ status, tracks_total: 10, tracks_done: 4 }), null);
});

test("budget text uses the smaller remainder", () => {
  const budget = (mine, total) => ({ mine: { limit: mine[0], used: mine[1], next_free_at: null }, total: { limit: total[0], used: total[1], next_free_at: null } });
  assert.equal(budgetText(budget([100, 40], [1000, 10])), "You can add 60 more tracks today.");
  assert.equal(budgetText(budget([100, 40], [1000, 980])), "You can add 20 more tracks today.");
  assert.equal(budgetText(budget([100, 99], [1000, 0])), "You can add 1 more track today.");
  assert.equal(budgetText(budget([100, 120], [1000, 0])), "You can add 0 more tracks today.");
  assert.equal(budgetText(budget([100, 0], [1000, 1000])), "You can add 0 more tracks today.");
  assert.equal(budgetText(null), null);
  assert.equal(budgetText({ mine: { limit: 10, used: 0 } }), null);
  assert.equal(budgetText(budget([10, -1], [10, 0])), null);
  assert.equal(budgetText(budget([1.5, 0], [10, 0])), null);
});

test("parseRoute falls back on bad input", () => {
  assert.deepEqual(parseRoute(""), { view: "queue" });
  assert.deepEqual(parseRoute("#"), { view: "queue" });
  assert.deepEqual(parseRoute("#/nope"), { view: "queue" });
  assert.deepEqual(parseRoute("#/album/0"), { view: "queue" });
  assert.deepEqual(parseRoute("#/album/0/%E0%A4%A"), { view: "queue" });
  assert.deepEqual(parseRoute("#/albums"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(parseRoute("#/albums?library=-1&list=bogus"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(parseRoute("#/artists?library=x"), { view: "artists", library: 0 });
  assert.deepEqual(parseRoute("#/album/zz/x"), { view: "album", library: 0, id: "x" });
  assert.deepEqual(parseRoute("#/similar/0"), { view: "queue" });
  assert.deepEqual(parseRoute("#/song/0"), { view: "queue" });
  assert.deepEqual(parseRoute("#/song/0/a/b"), { view: "queue" });
  assert.deepEqual(parseRoute("#/similar/0/a/b"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/0/a"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/0/a/b/c"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/0/a/%E0%A4%A"), { view: "queue" });
  assert.deepEqual(parseRoute("#/path/-3/a/b"), { view: "path", library: 0, from: "a", to: "b" });
});

test("routeForLibrary goes to the matching list view", () => {
  assert.deepEqual(routeForLibrary({ view: "albums", library: 0, list: "random" }, 1), { view: "albums", library: 1, list: "random" });
  assert.deepEqual(routeForLibrary({ view: "album", library: 0, id: "x" }, 1), { view: "albums", library: 1, list: "newest" });
  assert.deepEqual(routeForLibrary({ view: "song", library: 0, id: "x", namespace: "n" }, 1), { view: "albums", library: 1, list: "newest" });
  assert.deepEqual(routeForLibrary({ view: "artist", library: 0, id: "x" }, 1), { view: "artists", library: 1 });
  assert.deepEqual(routeForLibrary({ view: "playlist", library: 0, id: "x" }, 2), { view: "playlists", library: 2 });
  assert.deepEqual(routeForLibrary({ view: "queue" }, 1), { view: "queue" });
  assert.deepEqual(routeForLibrary({ view: "similar", library: 0, id: "x", namespace: "n" }, 1), { view: "queue" });
  assert.deepEqual(routeForLibrary({ view: "path", library: 0, from: "a", to: "b", namespace: "n" }, 0), { view: "queue" });
});

test("detail routes never carry old IDs into a restarted or remapped namespace", () => {
  for (const [view, safeView] of [["album", "albums"], ["artist", "artists"], ["playlist", "playlists"], ["song", "albums"]]) {
    const original = { view, library: 1, id: "old/id?", namespace: "page-a:0" };
    const route = parseRoute(buildRoute(original));
    assert.deepEqual(routeForNamespace(route, 1, "page-a:0"), original);
    const safe = { view: safeView, library: 1, ...(safeView === "albums" ? { list: "newest" } : {}) };
    assert.deepEqual(routeForNamespace(route, 1, null), safe);
    assert.deepEqual(routeForNamespace(route, 1, "page-a:1"), safe);
    assert.deepEqual(routeForNamespace(route, 1, "page-b:0"), safe);
    assert.deepEqual(routeForNamespace(parseRoute(buildRoute({ view, library: 1, id: original.id })), 1, "page-a:0"), safe);
    assert.deepEqual(routeForNamespace(route, 0, "page-a:0"), { ...safe, library: 0 });
  }
});

test("sonic routes never carry old song IDs into a restarted or remapped namespace", () => {
  for (const original of [
    { view: "similar", library: 1, id: "old/id?", namespace: "page-a:0" },
    { view: "path", library: 1, from: "a/b", to: "c?d", namespace: "page-a:0" },
  ]) {
    const route = parseRoute(buildRoute(original));
    assert.deepEqual(routeForNamespace(route, 1, "page-a:0"), original);
    assert.deepEqual(routeForNamespace(route, 1, null), { view: "queue" });
    assert.deepEqual(routeForNamespace(route, 1, "page-a:1"), { view: "queue" });
    assert.deepEqual(routeForNamespace(route, 1, "page-b:0"), { view: "queue" });
    const { namespace, ...unproved } = original;
    assert.deepEqual(routeForNamespace(parseRoute(buildRoute(unproved)), 1, namespace), { view: "queue" });
    assert.deepEqual(routeForNamespace(route, 0, "page-a:0"), { view: "queue" });
  }
});

test("cover retries double up to five seconds and then stop", () => {
  const delays = Array.from({ length: COVER_RETRIES }, (_, index) => coverRetryDelay(index + 1));
  assert.deepEqual(delays, [250, 500, 1000, 2000, 4000, 5000]);
  assert.equal(coverRetryDelay(COVER_RETRIES + 1), null);
  assert.equal(coverRetryDelay(0), null);
  assert.equal(coverRetryDelay(1.5), null);
});

test("similarity percent hides unknown scores", () => {
  assert.equal(similarityPercent(0.8), 80);
  assert.equal(similarityPercent(0.456), 46);
  assert.equal(similarityPercent(1), 100);
  assert.equal(similarityPercent(0), 0);
  assert.equal(similarityPercent(-1), null);
  assert.equal(similarityPercent(1.2), null);
  assert.equal(similarityPercent(null), null);
  assert.equal(similarityPercent(undefined), null);
  assert.equal(similarityPercent(NaN), null);
});

test("sonic path starts only from another song in the same library", () => {
  assert.equal(sonicPathStart({ library: 1, id: "now" }, { library: 1, id: "there" }), "now");
  assert.equal(sonicPathStart({ library: 1, id: "now" }, { library: 1, id: "now" }), null);
  assert.equal(sonicPathStart({ library: 0, id: "now" }, { library: 1, id: "there" }), null);
  assert.equal(sonicPathStart(null, { library: 1, id: "there" }), null);
  assert.equal(sonicPathStart({ library: 1, id: "" }, { library: 1, id: "there" }), null);
});

test("library reordering follows only an unambiguous old library key and drops detail IDs", () => {
  const previous = ["office", "home"];
  const current = ["home", "office"];
  const library = libraryForNamespace(previous, current, 1);
  assert.equal(library, 0);
  assert.deepEqual(routeForNamespace({ view: "playlist", library: 1, id: "old", namespace: "before" }, library, "after"), { view: "playlists", library: 0 });
  assert.deepEqual(routeForNamespace({ view: "albums", library: 1, list: "starred" }, library, "after"), { view: "albums", library: 0, list: "starred" });
  assert.equal(libraryForNamespace(["office", "home", "home"], ["office", "home"], 2), 0);
  assert.equal(libraryForNamespace(previous, ["office", "home", "home"], 1), 0);
  assert.equal(libraryForNamespace(previous, ["office", "replacement"], 1), 0);
  assert.equal(libraryForNamespace([], current, 1), 0);
});

test("Add and dragged items lose authority on disconnect and same-name restart", () => {
  const old = decodeDragItem(encodeDragItem("song", 1, "shared-id", "page-a:0"));
  assert.equal(browseItemIsCurrent(old, "page-a:0", 2), true);
  assert.equal(browseItemIsCurrent(old, null, 2), false);
  assert.equal(browseItemIsCurrent(old, "page-a:1", 2), false);
  assert.equal(browseItemIsCurrent(old, "page-b:0", 2), false);
  assert.equal(browseItemIsCurrent({ kind: "song", library: 1, id: "shared-id" }, "page-a:1", 2), false);
  const refreshed = decodeDragItem(encodeDragItem("song", 1, "shared-id", "page-a:1"));
  assert.equal(browseItemIsCurrent(refreshed, "page-a:1", 2), true);
  assert.equal(browseItemIsCurrent(refreshed, "page-a:1", 1), false);
  assert.equal(browseItemIsCurrent({ ...refreshed, library: -1 }, "page-a:1", 2), false);
  assert.equal(browseItemIsCurrent({ ...refreshed, library: undefined }, "page-a:1", 2), false);
});

test("debounce calls once with the last arguments", () => {
  mock.timers.enable({ apis: ["setTimeout"] });
  try {
    const calls = [];
    const fn = debounce((...args) => calls.push(args), 150);
    fn(1);
    fn(2);
    mock.timers.tick(149);
    assert.deepEqual(calls, []);
    fn(3, "x");
    mock.timers.tick(149);
    assert.deepEqual(calls, []);
    mock.timers.tick(1);
    assert.deepEqual(calls, [[3, "x"]]);
    fn(4);
    assert.equal(fn.pending(), true);
    fn.flush();
    assert.deepEqual(calls, [[3, "x"], [4]]);
    fn.flush();
    fn(5);
    fn.cancel();
    mock.timers.tick(500);
    assert.deepEqual(calls, [[3, "x"], [4]]);
    assert.equal(fn.pending(), false);
  } finally {
    mock.timers.reset();
  }
});

test("queue selection storage", () => {
  // The page path no longer carries the token; the key is per page root.
  assert.equal(queueStorageKey("/"), queueStorageKey("/"));
  assert.notEqual(queueStorageKey("/"), queueStorageKey("/prefix/"));
  assert.equal(selectedQueue(null, 3), 0);
  assert.equal(selectedQueue("2", 3), 2);
  assert.equal(selectedQueue("5", 3), 2);
  assert.equal(selectedQueue("-1", 3), 0);
  assert.equal(selectedQueue("1.5", 3), 0);
  assert.equal(selectedQueue("1", 0), 0);
});

test("drag items round trip and reject junk", () => {
  assert.deepEqual(decodeDragItem(encodeDragItem("album", 1, "a/b?c", "page:0")), { kind: "album", library: 1, id: "a/b?c", namespace: "page:0" });
  assert.equal(decodeDragItem("not json"), null);
  assert.equal(decodeDragItem(encodeDragItem("artist", 0, "x", "page:0")), null);
  assert.equal(decodeDragItem(encodeDragItem("song", -1, "x", "page:0")), null);
  assert.equal(decodeDragItem(encodeDragItem("song", 0, "", "page:0")), null);
  assert.equal(decodeDragItem(JSON.stringify({ kind: "song", library: 0, id: "old" })), null);
  assert.equal(decodeDragItem(encodeDragItem("song", 0, "x", "")), null);
  assert.equal(decodeDragItem("null"), null);
});

test("Space playback shortcut does not steal control keystrokes", () => {
  assert.equal(handlesSpace(null), false);
  assert.equal(handlesSpace({}), false);
  assert.equal(handlesSpace({ closest: () => null }), false);
  for (const control of ["input", "textarea", "select", "button", "a[href]", "[contenteditable='true']", "[role='slider']"]) {
    const target = { closest: (selector) => selector.split(", ").includes(control) ? target : null };
    assert.equal(handlesSpace(target), true, control);
  }
});

test("history refresh retains older pages, replaces duplicates and prunes expired plays", () => {
  const current = { items: [{ id: 6 }, { id: 5 }, { id: 4 }, { id: 3 }], next_before: 3 };
  const page = { items: [{ id: 7 }, { id: 6, updated: true }, { id: 5 }], next_before: 5, oldest_id: 4, revision: 7 };
  assert.deepEqual(mergeHistory(current, page), {
    items: [{ id: 7 }, { id: 6, updated: true }, { id: 5 }, { id: 4 }],
    next_before: null, oldest_id: 4, revision: 7,
  });
});

test("history refresh leaves a cursor for a gap before already loaded older plays", () => {
  const current = { items: [{ id: 3 }, { id: 2 }], next_before: 2 };
  const page = { items: [{ id: 8 }, { id: 7 }], next_before: 7, oldest_id: 1, revision: 8 };
  const merged = mergeHistory(current, page);
  assert.deepEqual(merged.items.map((item) => item.id), [8, 7, 3, 2]);
  assert.equal(merged.next_before, 7);
  const older = mergeHistory(merged, { items: [{ id: 6 }, { id: 5 }, { id: 4 }, { id: 3 }], next_before: 3, oldest_id: 1, revision: 8 }, true);
  assert.deepEqual(older.items.map((item) => item.id), [8, 7, 6, 5, 4, 3, 2]);
  assert.equal(older.next_before, 3);
});

test("having the oldest retained play does not hide a refresh gap", () => {
  const merged = mergeHistory({ items: [{ id: 2 }, { id: 1 }], next_before: null }, {
    items: [{ id: 8 }, { id: 7 }], next_before: 7, oldest_id: 1, revision: 8,
  });
  assert.equal(merged.next_before, 7);
});

const TOKEN = "3f2b8c1e-9d4a-4e6f-8b7a-0c1d2e3f4a5b";
const SHARE_IDS = ["plain", "a/b", "what?", "#hash", "with space", "50% & more", "ü/€?#=&+"];
const LIBRARY_VIEWS = new Set(["albums", "artists", "playlists", "genres", "genre", "album", "artist", "playlist", "song", "similar", "path"]);

function everyRoute(library, namespace) {
  const routes = [
    { view: "queue" }, { view: "channels" }, { view: "history" },
    { view: "search", q: "a b/c?d#e&f=g+h" },
    { view: "albums", library, list: "starred" }, { view: "artists", library }, { view: "playlists", library },
    { view: "genres", library }, { view: "genre", library, name: "R&B / Soul; 日本" },
    { view: "add", q: "a b/c?d#e&f=g+h" }, { view: "addAlbum", id: "0123456789abcdef" }, { view: "requests" },
  ];
  for (const id of SHARE_IDS) {
    for (const view of ["album", "artist", "playlist", "song", "similar"]) routes.push({ view, library, id, namespace });
    routes.push({ view: "path", library, from: id, to: SHARE_IDS[1], namespace });
  }
  return routes;
}

test("address and share hashes never carry the namespace; library routes carry their library key", () => {
  const keys = ["a1b2c3", "k&c/€", "9f8e7d"];
  for (const route of everyRoute(1, "session:7")) {
    for (const hash of [shareRoute(route, keys), addressRoute(route, keys)]) {
      assert.ok(!hash.includes("namespace"), hash);
      const params = new URLSearchParams(hash.slice(hash.indexOf("?") + 1));
      if (LIBRARY_VIEWS.has(route.view)) assert.equal(params.get("library_key"), "k&c/€", hash);
      else assert.ok(!hash.includes("library_key"), hash);
    }
  }
  // A duplicated or invalid library key cannot be shared; the address bar then shows the route without namespace.
  const album = { view: "album", library: 0, id: "x", namespace: "session:7" };
  assert.equal(shareRoute(album, ["same", "same"]), null);
  assert.equal(shareRoute(album, []), null);
  assert.equal(addressRoute(album, ["same", "same"]), "#/album/0/x");
  assert.equal(addressRoute({ ...album, sharedLibrary: "a1b2c3" }, ["a1b2c3"]), "#/album/0/x?library_key=a1b2c3");
});

test("share URLs use the page root and never leak a token", () => {
  const keys = ["a1b2c3", "9f8e7d"];
  for (const href of [
    `https://bot.example/prefix/#/album/0/x?namespace=s`,
    `https://bot.example/prefix/index.html?t=${TOKEN}#/queue`,
    `https://bot.example/prefix/u/${TOKEN}/#/album/0/x?namespace=s`,
    `https://bot.example/prefix/u/${TOKEN}/?token=${TOKEN}`,
    `https://user:${TOKEN}@bot.example/prefix/u/${TOKEN}/u/${TOKEN}/`,
  ]) {
    assert.equal(shareRoot(href), "https://bot.example/prefix/");
    for (const route of everyRoute(1, "s")) {
      const url = shareUrl(href, route, keys);
      assert.ok(url.startsWith("https://bot.example/prefix/#/"), url);
      assert.ok(!url.includes(TOKEN), url);
    }
  }
  assert.equal(shareRoot("http://127.0.0.1:8474/"), "http://127.0.0.1:8474/");
  assert.equal(shareUrl("http://h/", { view: "album", library: 0, id: "x" }, ["k", "k"]), null);
});

test("shared hashes parse with their library key and without namespace", () => {
  assert.deepEqual(parseRoute("#/album/3/a%2Fb?library_key=9f8e7d&namespace=old"), { view: "album", library: 3, id: "a/b", sharedLibrary: "9f8e7d" });
  assert.deepEqual(parseRoute("#/albums?library=2&list=random&library_key=k%26c"), { view: "albums", library: 2, list: "random", sharedLibrary: "k&c" });
  assert.deepEqual(parseRoute("#/search?q=x&library_key=9f8e7d"), { view: "search", q: "x" });
  assert.deepEqual(parseRoute("#/queue?library_key=9f8e7d"), { view: "queue" });
  // Library names are no longer part of shared links.
  assert.deepEqual(parseRoute("#/album/3/x?library_name=Jazz"), { view: "album", library: 3, id: "x" });
  assert.deepEqual(parseRoute("#/artists?library=1&library_name=Jazz"), { view: "artists", library: 1 });
});

test("shared, reloaded and back/forward hashes are adopted by library key", () => {
  const sender = ["main01", "jazz02", "rock03"];
  const receiver = ["rock03", "main01", "jazz02"];
  for (const route of everyRoute(1, "sender:0")) {
    const parsed = parseRoute(shareRoute(route, sender));
    const adopted = adoptSharedRoute(parsed, receiver, "receiver:4");
    if (!LIBRARY_VIEWS.has(route.view)) {
      assert.deepEqual(adopted, parsed);
      continue;
    }
    const expected = route.namespace === undefined ? { ...route, library: 2 } : { ...route, library: 2, namespace: "receiver:4" };
    assert.deepEqual(adopted, expected);
    // The adopted detail route is current for the receiver's namespace.
    assert.deepEqual(routeForNamespace(adopted, 2, "receiver:4"), expected);
    // Reload/back/forward: the address bar hash adopts back to the same route.
    assert.deepEqual(adoptSharedRoute(parseRoute(addressRoute(adopted, receiver)), receiver, "receiver:4"), expected);
  }
  const album = parseRoute("#/album/1/x?library_key=jazz02");
  assert.deepEqual(adoptSharedRoute(album, receiver, ""), album, "pending until a namespace exists");
  assert.deepEqual(adoptSharedRoute(album, ["main01"], "r:1"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(adoptSharedRoute(album, ["jazz02", "jazz02"], "r:1"), { view: "albums", library: 0, list: "newest" });
  // Same library name, new URL: the key differs, so the old item ID is not opened.
  assert.deepEqual(adoptSharedRoute(album, ["main01", "jazz99"], "r:1"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(adoptSharedRoute(parseRoute("#/similar/1/x?library_key=nope"), receiver, "r:1"), { view: "queue" });
  assert.deepEqual(adoptSharedRoute(parseRoute("#/artists?library=1&library_key=nope"), receiver, "r:1"), { view: "artists", library: 0 });
  assert.deepEqual(adoptSharedRoute(parseRoute("#/albums?library=0&list=starred&library_key=jazz02"), receiver, "r:1"), { view: "albums", library: 2, list: "starred" });
  // Internal routes without a library key keep the stale-namespace protection.
  assert.deepEqual(routeForNamespace(parseRoute("#/album/1/x?namespace=old"), 1, "r:1"), { view: "albums", library: 1, list: "newest" });
});

test("song routes keep the namespace in memory and the library key in the address bar", () => {
  const sender = ["main01", "jazz02"];
  const receiver = ["jazz02", "main01"];
  const song = { view: "song", library: 1, id: "s/1?", namespace: "sender:3" };
  assert.equal(buildRoute(song), "#/song/1/s%2F1%3F?namespace=sender%3A3");
  assert.deepEqual(parseRoute(buildRoute(song)), song);
  const shared = shareRoute(song, sender);
  assert.equal(shared, "#/song/1/s%2F1%3F?library_key=jazz02");
  assert.equal(addressRoute(song, sender), shared);
  assert.ok(!shareUrl("https://bot.example/", song, sender).includes("namespace"));
  const parsed = parseRoute(shared);
  assert.deepEqual(parsed, { view: "song", library: 1, id: "s/1?", sharedLibrary: "jazz02" });
  const adopted = adoptSharedRoute(parsed, receiver, "receiver:0");
  assert.deepEqual(adopted, { view: "song", library: 0, id: "s/1?", namespace: "receiver:0" });
  assert.deepEqual(routeForNamespace(adopted, 0, "receiver:0"), adopted);
  assert.equal(addressRoute(adopted, receiver), "#/song/0/s%2F1%3F?library_key=jazz02");
  // A missing library falls back to the album list; a stale namespace never opens the old song ID.
  assert.deepEqual(adoptSharedRoute(parsed, ["main01"], "receiver:0"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(routeForNamespace(adopted, 0, "receiver:1"), { view: "albums", library: 0, list: "newest" });
  assert.deepEqual(routeForNamespace(parseRoute("#/song/0/s1"), 0, "receiver:0"), { view: "albums", library: 0, list: "newest" });
});

test("playlists group into the caller's own, other people's and the library's", () => {
  const playlist = (id, fields = {}) => ({ id, name: id, personal: false, mine: false, favorites: false, shared: false, editable: false, ...fields });
  const list = [
    playlist("lib-b"), playlist("zebra", { personal: true, mine: true, editable: true }),
    playlist("bob-shared", { personal: true, shared: true, editable: true }), playlist("lib-a"),
    playlist("fav", { personal: true, mine: true, favorites: true, editable: true }),
    playlist("alpha", { personal: true, mine: true, editable: true }), playlist("ada-private", { personal: true }),
  ];
  const groups = groupPlaylists(list);
  assert.deepEqual(groups.mine.map((item) => item.id), ["fav", "alpha", "zebra"]);
  assert.deepEqual(groups.others.map((item) => item.id), ["ada-private", "bob-shared"]);
  assert.deepEqual(groups.library.map((item) => item.id), ["lib-b", "lib-a"]);
  assert.deepEqual(editablePlaylists(list).map((item) => item.id), ["fav", "alpha", "zebra", "bob-shared"]);
  assert.deepEqual(groupPlaylists(null), { mine: [], others: [], library: [] });
  assert.deepEqual(editablePlaylists(undefined), []);
});

test("the sung lyric line is the last one begun", () => {
  const lines = [{ start_ms: 1000 }, { start_ms: 4000 }, { start_ms: 4000 }, { start_ms: 9000 }];
  assert.equal(lyricLineAt(lines, 0), -1);
  assert.equal(lyricLineAt(lines, 999), -1);
  assert.equal(lyricLineAt(lines, 1000), 0);
  assert.equal(lyricLineAt(lines, 4500), 2);
  assert.equal(lyricLineAt(lines, 120000), 3);
  assert.equal(lyricLineAt([], 5), -1);
  assert.equal(lyricLineAt(null, 5), -1);
  assert.equal(lyricLineAt(lines, NaN), -1);
});

test("artist jumps name every group with artists by its position", () => {
  const groups = [{ name: "#", artists: [{ id: "1" }] }, { name: "A", artists: [] }, { name: "B", artists: [{ id: "2" }] }, { name: "Z", artists: [{ id: "3" }] }];
  assert.deepEqual(artistJumps(groups), [{ name: "#", index: 0 }, { name: "B", index: 2 }, { name: "Z", index: 3 }]);
  assert.deepEqual(artistJumps([{ name: "C" }]), []);
  assert.deepEqual(artistJumps(null), []);
});

test("suggestions start at two characters of trimmed text", () => {
  assert.equal(suggestionQuery("  ab "), "ab");
  assert.equal(suggestionQuery(" a "), null);
  assert.equal(suggestionQuery("ü"), null);
  assert.equal(suggestionQuery("😀😀"), "😀😀");
  assert.equal(suggestionQuery(""), null);
  assert.equal(suggestionQuery(undefined), null);
});

test("the highlighted suggestion steps and wraps", () => {
  assert.equal(suggestionStep(-1, 1, 4), 0);
  assert.equal(suggestionStep(-1, -1, 4), 3);
  assert.equal(suggestionStep(3, 1, 4), 0);
  assert.equal(suggestionStep(0, -1, 4), 3);
  assert.equal(suggestionStep(1, 1, 4), 2);
  assert.equal(suggestionStep(7, 1, 4), 0);
  assert.equal(suggestionStep(0, 1, 0), -1);
});

test("deck positions run at the deck's tempo and stop at the song's end", () => {
  assert.equal(deckPosition(1000, true, 8, 0, 1000, null), 2080);
  assert.equal(deckPosition(1000, true, -8, 500, 1500, null), 1920);
  assert.equal(deckPosition(1000, false, 8, 0, 1000, null), 1000);
  assert.equal(deckPosition(9000, true, 0, 0, 5000, 10), 10000);
  assert.equal(deckPosition(1000, true, Number.NaN, 0, 1000, null), 2000);
});

test("deck BPM follows the tempo and stays unknown without a library value", () => {
  assert.equal(deckBpm(120, 4), 124.8);
  assert.equal(deckBpm(128, -8), 117.8);
  assert.equal(deckBpm(174, 0), 174);
  for (const unknown of [null, undefined, 0, -3, Number.NaN]) assert.equal(deckBpm(unknown, 0), null);
});

test("crossfader gains match the bot's curves", () => {
  const [a, b] = crossfadeGains(0, "smooth");
  assert.ok(Math.abs(a - Math.SQRT1_2) < 1e-9 && Math.abs(b - Math.SQRT1_2) < 1e-9);
  assert.deepEqual(crossfadeGains(-1, "smooth"), [1, 0]);
  assert.deepEqual(crossfadeGains(-5, "smooth"), [1, 0]);
  assert.deepEqual(crossfadeGains(0, "cut"), [1, 1]);
  assert.deepEqual(crossfadeGains(-1, "cut"), [1, 0]);
  assert.deepEqual(crossfadeGains(1, "cut"), [0, 1]);
  const [full, half] = crossfadeGains(-0.96, "cut");
  assert.equal(full, 1);
  assert.ok(Math.abs(half - 0.5) < 1e-9);
});

test("the lead deck is the playing one, then the louder one, deck A on a tie", () => {
  const deck = (playing, volume = 100) => ({ entry: { id: 1 }, playing, volume });
  assert.equal(leadDeck({ crossfader: -1, curve: "smooth", decks: [deck(false), deck(true)] }), 1);
  assert.equal(leadDeck({ crossfader: -1, curve: "smooth", decks: [deck(true), deck(true)] }), 0);
  assert.equal(leadDeck({ crossfader: 0, curve: "smooth", decks: [deck(true, 40), deck(true)] }), 1);
  assert.equal(leadDeck({ crossfader: 0, curve: "smooth", decks: [deck(false), deck(false)] }), 0);
  assert.equal(leadDeck({ crossfader: 0, curve: "smooth", decks: [{ entry: null }, { entry: null }] }), null);
  assert.equal(leadDeck(null), null);
});

test("a running transition moves the crossfader to its deck in the remaining time", () => {
  assert.equal(transitionPosition(-1, 1, 0, 2000, 1000), 0);
  assert.equal(transitionPosition(0, 1, 1000, 2000, 1000), 1);
  assert.equal(transitionPosition(0, 1, 1000, 2000, 5000), 1);
  assert.equal(transitionPosition(0.5, 0, 0, 1000, -20), 0.5);
  assert.equal(transitionPosition(0.2, 0, 3000, 3000, 0), -1);
});

test("DJ control values read as text", () => {
  assert.equal(decibelText(0), "0.0 dB");
  assert.equal(decibelText(3.5), "+3.5 dB");
  assert.equal(decibelText(-2.4), "−2.5 dB");
  assert.equal(decibelText(-26), "−26 dB, cut");
  assert.equal(filterText(0), "Neutral");
  assert.equal(filterText(0.004), "Neutral");
  assert.equal(filterText(-0.4), "Low-pass 40 %");
  assert.equal(filterText(1), "High-pass 100 %");
  assert.equal(tempoText(2.5), "+2.50 %");
  assert.equal(tempoText(0), "0.00 %");
  assert.equal(tempoText(-8), "−8.00 %");
  assert.equal(crossfaderText(0), "Centre");
  assert.equal(crossfaderText(-1), "Deck A only");
  assert.equal(crossfaderText(1), "Deck B only");
  assert.equal(crossfaderText(0.25), "25 % towards deck B");
});

test("paced sends the newest value, one request at a time and at most once per interval", async () => {
  mock.timers.enable({ apis: ["setTimeout", "Date"], now: 0 });
  try {
    const sent = [];
    let finish = null;
    const send = paced((value) => {
      sent.push(value);
      return new Promise((resolve) => { finish = resolve; });
    }, 33);
    const settle = () => new Promise((resolve) => setImmediate(resolve));
    send(1);
    assert.deepEqual(sent, [1]);
    send(2);
    send(3);
    mock.timers.tick(100);
    assert.deepEqual(sent, [1], "the next request waits for the reply");
    finish();
    await settle();
    assert.deepEqual(sent, [1, 3], "values that waited collapse into the newest");
    finish();
    await settle();
    assert.equal(send.idle(), true);
    send(4);
    assert.deepEqual(sent, [1, 3]);
    mock.timers.tick(32);
    assert.deepEqual(sent, [1, 3]);
    mock.timers.tick(1);
    assert.deepEqual(sent, [1, 3, 4]);
    finish();
    await settle();
    send(5);
    send.cancel();
    mock.timers.tick(100);
    assert.deepEqual(sent, [1, 3, 4]);
    assert.equal(send.idle(), true);
  } finally {
    mock.timers.reset();
  }
});

test("MIDI can drive every DJ control once", () => {
  const controls = djControls();
  assert.equal(controls.length, 62);
  assert.equal(new Set(controls.map((control) => control.id)).size, controls.length);
  for (const control of controls) assert.ok(control.kind === "range" || control.kind === "button", control.id);
  assert.deepEqual(controls.filter((control) => control.kind === "range" && !control.center).map((control) => control.id), ["0.volume", "1.volume"]);
});

test("MIDI messages parse into control changes and notes", () => {
  assert.deepEqual(parseMidi([0xb3, 7, 100]), { type: "cc", channel: 3, number: 7, value: 100 });
  assert.deepEqual(parseMidi(new Uint8Array([0x90, 60, 127])), { type: "note", channel: 0, number: 60, value: 127 });
  assert.deepEqual(parseMidi([0x9f, 60, 0]), { type: "note", channel: 15, number: 60, value: 0 });
  assert.deepEqual(parseMidi([0x81, 60, 64]), { type: "note", channel: 1, number: 60, value: 0 });
  assert.equal(parseMidi([0xe0, 0, 64]), null, "pitch bend");
  assert.equal(parseMidi([0xf8]), null, "clock");
  assert.equal(parseMidi(null), null);
});

test("learning binds ranges to control changes, with 14 bits when the LSB follows", () => {
  const fader = { id: "0.volume", kind: "range" };
  const button = { id: "0.play", kind: "button" };
  const note = { type: "note", channel: 0, number: 11, value: 127 };
  const cc = (number, value = 64, channel = 0) => ({ type: "cc", channel, number, value });
  assert.equal(learnBinding(fader, [note]), null);
  assert.deepEqual(learnBinding(fader, [note, cc(7)]), { type: "cc", channel: 0, number: 7, mode: "absolute" });
  assert.deepEqual(learnBinding(fader, [cc(4), cc(36), cc(4)]), { type: "cc", channel: 0, number: 4, mode: "fine" });
  assert.deepEqual(learnBinding(fader, [cc(4), cc(36, 0, 2)]), { type: "cc", channel: 0, number: 4, mode: "absolute" });
  assert.deepEqual(learnBinding(fader, [cc(40), cc(72)]), { type: "cc", channel: 0, number: 40, mode: "absolute" });
  assert.deepEqual(learnBinding(button, [note]), { type: "note", channel: 0, number: 11, mode: "absolute" });
  assert.deepEqual(learnBinding(button, [cc(20, 127)]), { type: "cc", channel: 0, number: 20, mode: "absolute" });
  assert.equal(learnBinding(button, []), null);
});

test("MIDI values convert to fractions, steps and presses", () => {
  assert.equal(midiFraction(0), 0);
  assert.equal(midiFraction(127), 1);
  assert.equal(midiFraction(127, 127), 1);
  assert.equal(midiFraction(64, 0), 8192 / 16383);
  assert.equal(relativeDelta(1), 1);
  assert.equal(relativeDelta(3), 3);
  assert.equal(relativeDelta(127), -1);
  assert.equal(relativeDelta(125), -3);
  assert.equal(relativeDelta(64), 0);
  assert.equal(midiPressed({ type: "note", value: 1 }), true);
  assert.equal(midiPressed({ type: "note", value: 0 }), false);
  assert.equal(midiPressed({ type: "cc", value: 127 }), true);
  assert.equal(midiPressed({ type: "cc", value: 63 }), false);
});

test("control values rest in the middle of a knob's travel and invert", () => {
  assert.equal(controlValue("0.volume", 0), 0);
  assert.equal(controlValue("0.volume", 1), 100);
  assert.equal(controlValue("0.volume", 64 / 127), 50);
  for (const middle of [63 / 127, 64 / 127]) {
    assert.equal(controlValue("0.eq.low", middle), 0);
    assert.equal(controlValue("1.filter", middle), 0);
    assert.equal(controlValue("crossfader", middle), 0);
    assert.equal(controlValue("0.tempo", middle, 16), 0);
  }
  assert.equal(controlValue("0.eq.mid", 0), -26);
  assert.equal(controlValue("0.eq.high", 1), 6);
  assert.equal(controlValue("0.eq.high", 0.25), -13);
  assert.equal(controlValue("1.tempo", 1, 16), 16);
  assert.equal(controlValue("1.tempo", 0, 8), -8);
  assert.equal(controlValue("crossfader", 0), -1);
  assert.equal(controlValue("0.filter", 2), 1);
  for (const [id, value, range] of [["0.volume", 37, 8], ["0.eq.low", -13, 8], ["0.eq.low", 3, 8], ["1.tempo", 4, 16], ["crossfader", 0.5, 8], ["0.filter", -0.2, 8]]) {
    assert.ok(Math.abs(controlValue(id, controlFraction(id, value, range), range) - value) < 1e-9, `${id} ${value}`);
  }
});

test("stored mappings keep only known controls with valid bindings", () => {
  const mapping = {
    "0.volume": { type: "cc", channel: 0, number: 7, mode: "absolute" },
    "1.tempo": { type: "cc", channel: 1, number: 2, mode: "fine" },
    "0.play": { type: "note", channel: 0, number: 11, mode: "absolute" },
    crossfader: { type: "cc", channel: 0, number: 8, mode: "relative" },
  };
  assert.deepEqual(parseMapping(serializeMapping(mapping)), mapping);
  const stored = JSON.stringify({
    version: 1,
    controls: {
      ...mapping,
      "0.filter": { type: "note", channel: 0, number: 3 },
      "0.eq.low": { type: "cc", channel: 16, number: 3 },
      "0.eq.mid": { type: "cc", channel: 0, number: 40, mode: "fine" },
      "0.eq.high": { type: "cc", channel: 0, number: 41, mode: "loud" },
      "1.play": { type: "note", channel: 0, number: 12, mode: "relative" },
      "0.scratch": { type: "cc", channel: 0, number: 1 },
    },
  });
  assert.deepEqual(parseMapping(stored), {
    ...mapping,
    "0.eq.high": { type: "cc", channel: 0, number: 41, mode: "absolute" },
    "1.play": { type: "note", channel: 0, number: 12, mode: "absolute" },
  });
  for (const broken of [null, "", "{", "[]", JSON.stringify({ version: 2, controls: mapping }), "x".repeat(70000)]) {
    assert.deepEqual(parseMapping(broken), {}, String(broken).slice(0, 20));
  }
  assert.equal(midiStorageKey("DDJ-400"), "nuschl-musicbot:midi:DDJ-400");
  assert.equal(midiStorageKey("x".repeat(300)).length, "nuschl-musicbot:midi:".length + 200);
});

test("messages reach their bound controls, 14-bit ones through both halves", () => {
  const mapping = {
    "0.volume": { type: "cc", channel: 0, number: 7, mode: "absolute" },
    "1.tempo": { type: "cc", channel: 1, number: 2, mode: "fine" },
    "0.play": { type: "note", channel: 0, number: 7, mode: "absolute" },
  };
  assert.deepEqual(boundControls(mapping, { type: "cc", channel: 0, number: 7, value: 1 }).map((bound) => bound.id), ["0.volume"]);
  assert.deepEqual(boundControls(mapping, { type: "note", channel: 0, number: 7, value: 1 }).map((bound) => bound.id), ["0.play"]);
  assert.deepEqual(boundControls(mapping, { type: "cc", channel: 1, number: 34, value: 1 }), [{ id: "1.tempo", binding: mapping["1.tempo"], lsb: true }]);
  assert.deepEqual(boundControls(mapping, { type: "cc", channel: 1, number: 2, value: 1 })[0].lsb, false);
  assert.deepEqual(boundControls(mapping, { type: "cc", channel: 2, number: 7, value: 1 }), []);
  assert.equal(bindingText(mapping["1.tempo"]), "CC 2, channel 2, 14-bit");
  assert.equal(bindingText(mapping["0.play"]), "Note 7, channel 1");
  assert.equal(bindingText(undefined), "—");
});

test("MIDI offers loops of every beat length on both decks", () => {
  const ids = new Set(djControls().map((control) => control.id));
  for (const deck of [0, 1]) {
    for (const name of ["loopIn", "loopOut", "loopToggle", "loopHalve", "loopDouble", "loopBack", "loopForward", "quantize"]) assert.ok(ids.has(`${deck}.${name}`), name);
    for (const beats of DJ_BEAT_LOOPS) assert.ok(ids.has(`${deck}.beatLoop.${beats}`), String(beats));
  }
  assert.deepEqual(DJ_BEAT_LOOPS.map(beatsText), ["1/4 beat", "1/2 beat", "1 beat", "2 beats", "4 beats", "8 beats", "16 beats", "32 beats"]);
});

test("default DJ keys cover every action once, with a deck B mirror of deck A", () => {
  const actions = djKeyActions();
  assert.equal(new Set(actions.map((action) => action.id)).size, actions.length);
  const defaults = defaultKeyBindings(actions);
  assert.deepEqual(findConflicts(defaults), []);
  assert.ok(Object.values(defaults).every((binding) => binding.code !== "Space"), "Space stays play and pause");
  const deckA = actions.filter((action) => action.deck === 0).map((action) => action.name);
  const deckB = actions.filter((action) => action.deck === 1).map((action) => action.name);
  assert.deepEqual(deckA, deckB);
  assert.deepEqual([...new Set(actions.map((action) => action.group))], ["Deck A", "Mixer", "Deck B"]);
  assert.deepEqual(defaults["0.play"], { code: "KeyD", shift: false });
  assert.deepEqual(defaults["1.setCue"], { code: "Semicolon", shift: true });
  assert.deepEqual(defaults.help, { code: "Slash", shift: true });
  for (const action of actions) {
    if (action.glide) assert.ok(Object.hasOwn(DJ_KEY_GLIDES, action.glide.control), action.id);
  }
});

test("continuous keys step per press and glide while held", () => {
  assert.equal(keyStep("volume", 50, 1), 55);
  assert.equal(keyStep("volume", 98, 1), 100);
  assert.equal(keyStep("volume", 2, -1), 0);
  assert.equal(keyStep("tempo", 0.37, 1, 8), 0.47);
  assert.equal(keyStep("tempo", 7.95, 1, 8), 8);
  assert.equal(keyStep("tempo", -15.95, -1, 16), -16);
  assert.equal(keyStep("filter", 0, -1), -0.05);
  assert.equal(keyStep("crossfader", 0.98, 1), 1);
  assert.equal(glideValue("crossfader", 0, 1, DJ_KEY_GLIDE_DELAY_MS), 0.05);
  assert.equal(glideValue("crossfader", 0, 1, DJ_KEY_GLIDE_DELAY_MS + 500), 0.55);
  assert.equal(glideValue("crossfader", 0, -1, 60000), -1);
  assert.equal(glideValue("volume", 50, -1, DJ_KEY_GLIDE_DELAY_MS + 200), 35);
  assert.equal(glideValue("tempo", 0, 1, DJ_KEY_GLIDE_DELAY_MS + 1000, 8), 1.1);
  assert.equal(glideValue("filter", Number.NaN, 1, 0), 0.05);
});

test("EQ kill keys cut a band and restore its value", () => {
  assert.equal(eqKillTarget(0, undefined), -26);
  assert.equal(eqKillTarget(3.5, undefined), -26);
  assert.equal(eqKillTarget(-26, 3.5), 3.5);
  assert.equal(eqKillTarget(-26, undefined), 0);
  assert.equal(eqKillTarget(-26, -26), 0);
});

test("key labels read as the keyboard prints them", () => {
  assert.equal(keyLabel({ code: "KeyD", shift: false }), "D");
  assert.equal(keyLabel({ code: "Semicolon", shift: true }), "Shift + ;");
  assert.equal(keyLabel({ code: "Digit0", shift: false }), "0");
  assert.equal(keyLabel({ code: "Space", shift: false }), "Space");
  assert.equal(keyLabel({ code: "PageUp", shift: true }), "Shift + Page Up");
  assert.equal(keyLabel({ code: "Numpad4", shift: false }), "Num 4");
  assert.equal(keyLabel(null), "—");
  const german = new Map([["Semicolon", "ö"], ["Slash", "-"], ["Space", " "]]);
  assert.equal(keyLabel({ code: "Semicolon", shift: true }, german), "Shift + Ö");
  assert.equal(keyLabel({ code: "Slash", shift: false }, german), "-");
  assert.equal(keyLabel({ code: "Space", shift: false }, german), "Space");
});

test("key events become shortcut keys only without other modifiers", () => {
  const event = (code, extra = {}) => ({ code, shiftKey: false, ctrlKey: false, altKey: false, metaKey: false, isComposing: false, ...extra });
  assert.deepEqual(keyFromEvent(event("KeyF")), { code: "KeyF", shift: false });
  assert.deepEqual(keyFromEvent(event("KeyF", { shiftKey: true })), { code: "KeyF", shift: true });
  assert.deepEqual(keyFromEvent(event("Space")), { code: "Space", shift: false });
  for (const extra of [{ ctrlKey: true }, { altKey: true }, { metaKey: true }, { isComposing: true }]) assert.equal(keyFromEvent(event("KeyF", extra)), null);
  for (const code of ["ShiftLeft", "ControlRight", "Escape", "Tab", "", "keyf", "Key F", undefined]) assert.equal(keyFromEvent(event(code)), null, String(code));
  assert.equal(keyFromEvent(null), null);
  assert.equal(keyMatches({ code: "KeyF", shift: true }, { code: "KeyF", shift: true }), true);
  assert.equal(keyMatches({ code: "KeyF", shift: true }, { code: "KeyF", shift: false }), false);
  assert.equal(keyMatches(null, { code: "KeyF", shift: false }), false);
  assert.equal(keyMatches({ code: "KeyF", shift: false }, null), false);
});

test("binding a key moves it from the action that held it", () => {
  const bindings = { a: { code: "KeyD", shift: false }, b: { code: "KeyF", shift: false }, c: { code: "KeyF", shift: true }, d: null };
  const { bindings: next, moved } = bindKey(bindings, "a", { code: "KeyF", shift: false });
  assert.deepEqual(moved, ["b"]);
  assert.deepEqual(next, { a: { code: "KeyF", shift: false }, b: null, c: { code: "KeyF", shift: true }, d: null });
  assert.deepEqual(bindKey(bindings, "d", { code: "Space", shift: false }).moved, []);
  assert.deepEqual(bindKey(bindings, "a", { code: "KeyD", shift: false }).moved, []);
  assert.deepEqual(findConflicts({ a: { code: "KeyF", shift: false }, b: { code: "KeyF", shift: false }, c: null }), [{ key: { code: "KeyF", shift: false }, actions: ["a", "b"] }]);
});

test("stored key bindings are validated and filled with free defaults", () => {
  const actions = djKeyActions();
  const defaults = defaultKeyBindings(actions);
  assert.deepEqual(parseKeyBindings(null, actions), defaults);
  assert.deepEqual(parseKeyBindings("not json", actions), defaults);
  assert.deepEqual(parseKeyBindings(JSON.stringify({ version: 2, keys: { "0.play": null } }), actions), defaults);
  assert.deepEqual(parseKeyBindings(serializeKeyBindings(defaults), actions), defaults);
  assert.deepEqual(parseKeyBindings("x".repeat(20000), actions), defaults);

  // Stored keys win; malformed keys and a second use of a key fall back to the free default.
  const stored = serializeKeyBindings({
    "0.play": { code: "Space", shift: false },
    "0.cue": null,
    "0.sync": { code: "KeyF", shift: false },
    "0.tempoUp": { code: "Escape", shift: false },
    "0.tempoDown": { code: "KeyQ", shift: "yes" },
    "1.play": { code: "Space", shift: false },
    unknown: { code: "KeyU", shift: false },
  });
  const parsed = parseKeyBindings(stored, actions);
  assert.equal(Object.keys(parsed).length, actions.length);
  assert.deepEqual(parsed["0.play"], { code: "Space", shift: false });
  assert.equal(parsed["0.cue"], null);
  assert.deepEqual(parsed["0.sync"], { code: "KeyF", shift: false });
  assert.deepEqual(parsed["0.tempoUp"], defaults["0.tempoUp"]);
  assert.deepEqual(parsed["0.tempoDown"], defaults["0.tempoDown"]);
  assert.deepEqual(parsed["1.play"], defaults["1.play"]);
  assert.equal(Object.hasOwn(parsed, "unknown"), false);
  assert.deepEqual(findConflicts(parsed), []);
});

test("shortcuts pause in fields that take typed text", () => {
  assert.equal(typingTarget(null), false);
  assert.equal(typingTarget({ tagName: "BUTTON" }), false);
  assert.equal(typingTarget({ tagName: "DIV", isContentEditable: true }), true);
  assert.equal(typingTarget({ tagName: "TEXTAREA" }), true);
  assert.equal(typingTarget({ tagName: "SELECT" }), true);
  for (const type of ["text", "search", "number", "email", "password", "url", "", undefined]) assert.equal(typingTarget({ tagName: "INPUT", type }), true, String(type));
  for (const type of ["range", "checkbox", "radio", "button"]) assert.equal(typingTarget({ tagName: "INPUT", type }), false, type);
});

test("spectrum bytes map to dB and bar heights", () => {
  assert.equal(levelDb(0), -72);
  assert.equal(levelDb(255), 0);
  assert.ok(Math.abs(levelDb(170) + 24) < 1e-9);
  assert.equal(levelHeight(0), 0);
  assert.equal(levelHeight(63), 0);
  assert.equal(levelHeight(255), 1);
  assert.ok(levelHeight(170) > 0.5 && levelHeight(170) < 0.6);
});

test("the waveform window and its pixel mapping centre the playhead", () => {
  assert.deepEqual(waveWindow(60000, 6), { from: 5400, to: 6600 });
  assert.deepEqual(waveWindow(1005, 3), { from: -200, to: 401 });
  assert.equal(timeX(60000, 60000, 6, 1200), 600);
  assert.equal(timeX(54000, 60000, 6, 1200), 0);
  assert.equal(timeX(66000, 60000, 6, 1200), 1200);
  assert.equal(timeX(63000, 60000, 3, 600), 600);
  for (const x of [0, 137, 600, 1200]) assert.ok(Math.abs(timeX(xTime(x, 42000, 12, 1200), 42000, 12, 1200) - x) < 1e-9);
});

test("rangesToFetch requests the uncached columns below the computed count", () => {
  assert.deepEqual(rangesToFetch([], { from: -100, to: 500 }, null), [{ from: 0, to: 500 }]);
  assert.deepEqual(rangesToFetch([{ from: 100, to: 200 }, { from: 0, to: 50 }], { from: 0, to: 300 }, null), [{ from: 50, to: 100 }, { from: 200, to: 300 }]);
  assert.deepEqual(rangesToFetch([{ from: 0, to: 300 }], { from: 100, to: 200 }, null), []);
  assert.deepEqual(rangesToFetch([{ from: 0, to: 100 }], { from: 0, to: 1000 }, 400), [{ from: 100, to: 400 }]);
  assert.deepEqual(rangesToFetch([], { from: 500, to: 1000 }, 400), []);
  assert.deepEqual(rangesToFetch([], { from: 0, to: 2 * SPECTRUM_MAX_COLUMNS + 10 }, null), [
    { from: 0, to: SPECTRUM_MAX_COLUMNS }, { from: SPECTRUM_MAX_COLUMNS, to: 2 * SPECTRUM_MAX_COLUMNS }, { from: 2 * SPECTRUM_MAX_COLUMNS, to: 2 * SPECTRUM_MAX_COLUMNS + 10 },
  ]);
  assert.deepEqual(rangesToFetch([{ from: 10, to: 20 }], { from: 0, to: 30 }, null, 5), [{ from: 0, to: 5 }, { from: 5, to: 10 }, { from: 20, to: 25 }, { from: 25, to: 30 }]);
});

test("cached spectrum ranges stay sorted, disjoint and bounded", () => {
  let ranges = insertRange([], { from: 100, to: 200 });
  ranges = insertRange(ranges, { from: 0, to: 50 });
  ranges = insertRange(ranges, { from: 300, to: 400 });
  assert.deepEqual(ranges.map((range) => range.from), [0, 100, 300]);
  assert.equal(findRange(ranges, 0), 0);
  assert.equal(findRange(ranges, 49), 0);
  assert.equal(findRange(ranges, 50), -1);
  assert.equal(findRange(ranges, 199), 1);
  assert.equal(findRange(ranges, 350), 2);
  assert.equal(findRange(ranges, 400), -1);
  assert.equal(findRange([], 5), -1);
  assert.deepEqual(insertRange(ranges, { from: 150, to: 320 }).map((range) => [range.from, range.to]), [[0, 50], [150, 320]]);

  const many = [{ from: 0, to: 1000 }, { from: 1000, to: 2000 }, { from: 5000, to: 6000 }, { from: 20000, to: 21000 }];
  assert.deepEqual(pruneRanges(many, 1500, 6000, 30000).map((range) => range.from), [0, 1000, 5000]);
  assert.deepEqual(pruneRanges(many, 1500, 6000, 2000).map((range) => range.from), [0, 1000]);
  assert.deepEqual(pruneRanges(many, 1500, 6000, 10).map((range) => range.from), [1000]);
  assert.deepEqual(pruneRanges(many, 50000, 6000, 30000), []);
});

test("theme colours parse and spread over a heat map", () => {
  assert.deepEqual(parseColour("#126c50"), [18, 108, 80]);
  assert.deepEqual(parseColour(" #FFF "), [255, 255, 255]);
  assert.deepEqual(parseColour("rgb(1, 2, 3)"), [1, 2, 3]);
  assert.deepEqual(parseColour("rgba(10 20 30 / 50%)"), [10, 20, 30]);
  assert.equal(parseColour("teal"), null);
  assert.equal(parseColour(""), null);
  const stops = [[0, 0, 0], [100, 0, 0], [100, 200, 0]];
  assert.deepEqual(heatColour(0, stops), [0, 0, 0]);
  assert.deepEqual(heatColour(255, stops), [100, 200, 0]);
  assert.deepEqual(heatColour(127.5, stops), [100, 0, 0]);
  assert.deepEqual(heatColour(300, stops), [100, 200, 0]);
  assert.deepEqual(heatColour(51, [[0, 0, 0], [255, 255, 255]]), [51, 51, 51]);
});

test("gridTicks marks bars and thins out long spans", () => {
  assert.deepEqual(gridTicks(null, 0, 1000, 10), []);
  assert.deepEqual(gridTicks({ bpm: 0, first_beat_ms: 0 }, 0, 1000, 10), []);
  const ticks = gridTicks({ bpm: 120, first_beat_ms: 250 }, 0, 2500, 100);
  assert.deepEqual(ticks.map((tick) => tick.ms), [250, 750, 1250, 1750, 2250]);
  assert.deepEqual(ticks.map((tick) => tick.bar), [true, false, false, false, true]);
  assert.deepEqual(gridTicks({ bpm: 120, first_beat_ms: 250 }, -300, 0, 100).map((tick) => [tick.ms, tick.bar]), [[-250, false]]);
  const thinned = gridTicks({ bpm: 120, first_beat_ms: 0 }, 0, 600000, 200);
  assert.ok(thinned.length <= 200);
  assert.equal(thinned[1].ms - thinned[0].ms, 8000);
  assert.ok(thinned.every((tick) => tick.bar));
});
