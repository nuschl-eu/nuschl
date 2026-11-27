// SPDX-License-Identifier: EUPL-1.2
//! The playlist endpoints against a library on loopback that keeps its
//! playlists in memory, like Navidrome's Subsonic API.
use super::*;
use crate::{
    subsonic::tests::Server,
    tokens::Owner,
    web::tests::{API_TESTS, test_bot},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{io::Write, sync::Mutex, time::Duration};

#[derive(Default)]
struct Fake {
    /// id, name, public, songs
    playlists: Vec<(String, String, bool, Vec<String>)>,
    made: usize,
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            out.push(u8::from_str_radix(&text[index + 1..index + 3], 16).unwrap());
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn library() -> (Server, Arc<Mutex<Fake>>) {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let server = Server::new({
        let fake = fake.clone();
        move |request, socket| {
            let target = request.split_whitespace().nth(1).unwrap();
            let (path, query) = target.split_once('?').unwrap();
            let params: Vec<(&str, String)> = query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (key, decode(value)))
                .collect();
            let one = |name: &str| {
                params
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            };
            let all = |name: &str| -> Vec<String> {
                params
                    .iter()
                    .filter(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
                    .collect()
            };
            let mut fake = fake.lock().unwrap();
            let not_found =
                json!({"status": "failed", "error": {"code": 70, "message": "not found"}});
            let reply = match path {
                "/rest/getPlaylists" => json!({"status": "ok", "playlists": {"playlist":
                    fake.playlists.iter().map(|(id, name, _, songs)| json!({
                        "id": id, "name": name, "songCount": songs.len(), "owner": "bot",
                    })).collect::<Vec<_>>()}}),
                "/rest/getPlaylist" => {
                    let id = one("id").unwrap();
                    match fake.playlists.iter().find(|playlist| playlist.0 == id) {
                        Some((id, name, _, songs)) => json!({"status": "ok", "playlist": {
                            "id": id, "name": name, "songCount": songs.len(), "owner": "bot",
                            "entry": songs.iter().map(|song| json!({"id": song, "title": song})).collect::<Vec<_>>(),
                        }}),
                        None => not_found,
                    }
                }
                "/rest/createPlaylist" => {
                    fake.made += 1;
                    let id = format!("p{}", fake.made);
                    let name = one("name").unwrap();
                    // New playlists start public here, so the test sees the bot make them private.
                    fake.playlists
                        .push((id.clone(), name.clone(), true, Vec::new()));
                    json!({"status": "ok", "playlist": {"id": id, "name": name, "songCount": 0}})
                }
                "/rest/updatePlaylist" => {
                    let id = one("playlistId").unwrap();
                    match fake.playlists.iter_mut().find(|playlist| playlist.0 == id) {
                        Some(playlist) => {
                            if let Some(name) = one("name") {
                                playlist.1 = name;
                            }
                            if let Some(public) = one("public") {
                                playlist.2 = public == "true";
                            }
                            let mut removed: Vec<usize> = all("songIndexToRemove")
                                .iter()
                                .map(|index| index.parse().unwrap())
                                .collect();
                            removed.sort_unstable_by(|a, b| b.cmp(a));
                            for index in removed {
                                playlist.3.remove(index);
                            }
                            playlist.3.extend(all("songIdToAdd"));
                            json!({"status": "ok"})
                        }
                        None => not_found,
                    }
                }
                "/rest/deletePlaylist" => {
                    let id = one("id").unwrap();
                    fake.playlists.retain(|playlist| playlist.0 != id);
                    json!({"status": "ok"})
                }
                "/rest/getAlbum" => json!({"status": "ok", "album": {
                    "id": one("id").unwrap(), "name": "Album",
                    "song": [{"id": "a1"}, {"id": "a2"}],
                }}),
                other => panic!("unexpected endpoint {other}"),
            };
            let body = serde_json::to_vec(&json!({"subsonic-response": reply})).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(&body).unwrap();
        }
    });
    (server, fake)
}

/// Ada and Bob have certificates; Cy has none.
fn people(bot: &Bot) -> [String; 3] {
    [("Ada", Some("aa11")), ("Bob", Some("bb22")), ("Cy", None)].map(|(name, certificate)| {
        bot.tokens
            .issue(
                Owner {
                    server: "mumble.example:64738".into(),
                    name: name.into(),
                    certificate: certificate.map(str::to_owned),
                },
                false,
            )
            .unwrap()
            .to_string()
    })
}

fn body<T: DeserializeOwned>(value: Value) -> Json<T> {
    Json(serde_json::from_value(value).unwrap())
}

fn value<T: Serialize>(item: &T) -> Value {
    serde_json::to_value(item).unwrap()
}

fn refused<T>(result: Result<T, ApiError>) -> (StatusCode, String) {
    match result {
        Ok(_) => panic!("the request was accepted"),
        Err(ApiError(status, message)) => (status, message),
    }
}

fn songs(fake: &Mutex<Fake>, id: &str) -> Vec<String> {
    let fake = fake.lock().unwrap();
    fake.playlists
        .iter()
        .find(|playlist| playlist.0 == id)
        .map(|playlist| playlist.3.clone())
        .unwrap_or_default()
}

fn endpoints(server: &Server) -> Vec<String> {
    server
        .requests()
        .iter()
        .map(|request| {
            let target = request.split_whitespace().nth(1).unwrap();
            target
                .split('?')
                .next()
                .unwrap()
                .trim_start_matches("/rest/")
                .to_owned()
        })
        .collect()
}

async fn listing(bot: &Arc<Bot>, token: &str) -> Vec<Value> {
    let list = list(
        State(bot.clone()),
        Path(token.to_owned()),
        Query(LibraryQuery { library: 0 }),
    )
    .await
    .ok()
    .unwrap();
    value(&list.0)["playlists"].as_array().unwrap().clone()
}

async fn add_songs(
    bot: &Arc<Bot>,
    token: &str,
    id: &str,
    songs: &[&str],
) -> Result<usize, ApiError> {
    add(
        State(bot.clone()),
        Path(token.to_owned()),
        body(json!({"library": 0, "id": id, "songs": songs})),
    )
    .await
    .map(|added| added.0.added)
}

async fn make(bot: &Arc<Bot>, token: &str, name: &str) -> Result<String, ApiError> {
    create(
        State(bot.clone()),
        Path(token.to_owned()),
        body(json!({"library": 0, "name": name})),
    )
    .await
    .map(|item| item.0.playlist.id)
}

#[tokio::test]
async fn owners_manage_shared_playlists_take_edits_and_certificates_are_required() {
    let _exclusive = API_TESTS.lock().await;
    let (server, fake) = library();
    let (_directory, bot, _token) = test_bot(&server.url, 1);
    let [ada, bob, cy] = people(&bot);

    let made = create(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "name": "  Road trip "})),
    )
    .await
    .ok()
    .unwrap();
    let made = value(&made.0);
    assert_eq!(made["name"], "Road trip");
    for (field, expected) in [
        ("personal", json!(true)),
        ("mine", json!(true)),
        ("editable", json!(true)),
        ("manageable", json!(true)),
        ("shared", json!(false)),
        ("favorites", json!(false)),
        ("owner_name", json!("Ada")),
    ] {
        assert_eq!(made[field], expected, "{field}");
    }
    let id = made["id"].as_str().unwrap().to_owned();
    assert_eq!(endpoints(&server), ["createPlaylist", "updatePlaylist"]);
    assert!(
        !fake.lock().unwrap().playlists[0].2,
        "kept private in the library"
    );
    assert_eq!(bot.playlists.revision(), 1);
    let entry = bot.playlists.get(bot.libraries[0].key(), &id).unwrap();
    assert_eq!(
        entry.owner,
        bot.pseudonyms.of(&owner(&bot, &ada).ok().unwrap())
    );

    // Others see whose it is and leave it alone while it is private.
    let seen = &listing(&bot, &bob).await[0];
    assert_eq!(
        (&seen["owner_name"], &seen["mine"], &seen["editable"]),
        (&json!("Ada"), &json!(false), &json!(false))
    );
    assert_eq!(
        refused(add_songs(&bot, &bob, &id, &["s1"]).await),
        (StatusCode::FORBIDDEN, PRIVATE.to_owned())
    );
    let renamed = rename(
        State(bot.clone()),
        Path(bob.clone()),
        body(json!({"library": 0, "id": id, "name": "Mine now"})),
    )
    .await;
    assert_eq!(
        refused(renamed),
        (StatusCode::FORBIDDEN, OWNER_ONLY.to_owned())
    );

    // Without a certificate: viewing only.
    assert_eq!(
        refused(make(&bot, &cy, "Mine").await),
        (StatusCode::FORBIDDEN, NO_CERTIFICATE.to_owned())
    );
    let cy_view = owner(&bot, &cy).ok().unwrap();
    assert!(
        !value(&super::super::snapshot(&bot, &cy_view))["playlists"]["can_own"]
            .as_bool()
            .unwrap()
    );
    let ada_view = owner(&bot, &ada).ok().unwrap();
    assert_eq!(
        value(&super::super::snapshot(&bot, &ada_view))["playlists"],
        json!({"revision": 1, "can_own": true})
    );

    // Shared: Bob edits the songs, Cy still only views.
    share(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id, "shared": true})),
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(
        add_songs(&bot, &bob, &id, &["s1", "s2"]).await.ok(),
        Some(2)
    );
    assert_eq!(songs(&fake, &id), ["s1", "s2"]);
    assert_eq!(
        refused(add_songs(&bot, &cy, &id, &["s3"]).await),
        (StatusCode::FORBIDDEN, NO_CERTIFICATE.to_owned())
    );
    let seen = &listing(&bot, &cy).await[0];
    assert_eq!(
        (&seen["shared"], &seen["editable"]),
        (&json!(true), &json!(false))
    );
    let seen = &listing(&bot, &bob).await[0];
    assert_eq!(
        (&seen["editable"], &seen["manageable"]),
        (&json!(true), &json!(false))
    );
    let added = add(
        State(bot.clone()),
        Path(bob.clone()),
        body(json!({"library": 0, "id": id, "album": "al"})),
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(added.0.added, 2);
    assert_eq!(songs(&fake, &id), ["s1", "s2", "a1", "a2"]);
    remove(
        State(bot.clone()),
        Path(bob.clone()),
        body(json!({"library": 0, "id": id, "index": 0, "song": "s1"})),
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(songs(&fake, &id), ["s2", "a1", "a2"]);
    for (handler, request) in [
        ("share", json!({"library": 0, "id": id, "shared": false})),
        ("delete", json!({"library": 0, "id": id})),
    ] {
        let result = match handler {
            "share" => share(State(bot.clone()), Path(bob.clone()), body(request)).await,
            _ => delete(State(bot.clone()), Path(bob.clone()), body(request)).await,
        };
        assert_eq!(
            refused(result),
            (StatusCode::FORBIDDEN, OWNER_ONLY.to_owned())
        );
    }
    let queue = add(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id, "player": 0})),
    )
    .await;
    assert_eq!(refused(queue).0, StatusCode::BAD_REQUEST, "no queues here");

    // Playlists made in the library stay read-only.
    fake.lock().unwrap().playlists.push((
        "lib".into(),
        "Library mix".into(),
        true,
        vec!["x".into()],
    ));
    let seen = listing(&bot, &ada).await;
    let library_playlist = seen
        .iter()
        .find(|playlist| playlist["id"] == "lib")
        .unwrap();
    assert_eq!(
        (
            &library_playlist["personal"],
            &library_playlist["editable"],
            &library_playlist["owner"]
        ),
        (&json!(false), &json!(false), &json!("bot"))
    );
    assert_eq!(
        refused(add_songs(&bot, &ada, "lib", &["s1"]).await),
        (StatusCode::FORBIDDEN, LIBRARY_PLAYLIST.to_owned())
    );

    // The owner renames and deletes.
    rename(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id, "name": "Trip"})),
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(fake.lock().unwrap().playlists[0].1, "Trip");
    let revision = bot.playlists.revision();
    delete(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id})),
    )
    .await
    .ok()
    .unwrap();
    assert!(bot.playlists.revision() > revision);
    assert_eq!(bot.playlists.get(bot.libraries[0].key(), &id), None);
    assert_eq!(fake.lock().unwrap().playlists.len(), 1);
}

#[tokio::test]
async fn favorites_are_made_once_and_toggle_the_song() {
    let _exclusive = API_TESTS.lock().await;
    let (server, fake) = library();
    let (_directory, bot, _token) = test_bot(&server.url, 1);
    let [ada, bob, cy] = people(&bot);
    async fn is_favorite(bot: &Arc<Bot>, token: &str, song: &str) -> bool {
        favorite(
            State(bot.clone()),
            Path(token.to_owned()),
            Query(ItemQuery {
                library: 0,
                id: song.into(),
            }),
        )
        .await
        .ok()
        .unwrap()
        .0
        .favorite
    }
    async fn set(bot: &Arc<Bot>, token: &str, song: &str, on: bool) -> Result<bool, ApiError> {
        set_favorite(
            State(bot.clone()),
            Path(token.to_owned()),
            body(json!({"library": 0, "id": song, "favorite": on})),
        )
        .await
        .map(|view| view.0.favorite)
    }

    assert!(!is_favorite(&bot, &ada, "s1").await);
    assert_eq!(set(&bot, &ada, "s1", false).await.ok(), Some(false));
    assert!(
        server.requests().is_empty(),
        "no Favorites, no library call"
    );

    assert_eq!(set(&bot, &ada, "s1", true).await.ok(), Some(true));
    assert_eq!(set(&bot, &ada, "s1", true).await.ok(), Some(true));
    assert_eq!(set(&bot, &ada, "s2", true).await.ok(), Some(true));
    let made: Vec<_> = endpoints(&server)
        .into_iter()
        .filter(|endpoint| endpoint == "createPlaylist")
        .collect();
    assert_eq!(made.len(), 1, "made on first use only");
    let (id, name) = {
        let fake = fake.lock().unwrap();
        (fake.playlists[0].0.clone(), fake.playlists[0].1.clone())
    };
    assert_eq!(name, "Favorites of Ada");
    assert_eq!(songs(&fake, &id), ["s1", "s2"]);
    assert!(is_favorite(&bot, &ada, "s1").await);
    assert!(!is_favorite(&bot, &bob, "s1").await, "Bob has his own");

    // A copy added through the playlist page goes too.
    add_songs(&bot, &ada, &id, &["s1"]).await.ok().unwrap();
    assert_eq!(set(&bot, &ada, "s1", false).await.ok(), Some(false));
    assert_eq!(songs(&fake, &id), ["s2"]);
    assert!(!is_favorite(&bot, &ada, "s1").await);

    let seen = &listing(&bot, &ada).await[0];
    assert_eq!(
        (&seen["favorites"], &seen["editable"], &seen["manageable"]),
        (&json!(true), &json!(true), &json!(false))
    );
    let shared = share(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id, "shared": true})),
    )
    .await;
    assert_eq!(
        refused(shared),
        (StatusCode::FORBIDDEN, FAVORITES_FIXED.to_owned())
    );
    let deleted = delete(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id})),
    )
    .await;
    assert_eq!(
        refused(deleted),
        (StatusCode::FORBIDDEN, FAVORITES_FIXED.to_owned())
    );
    assert_eq!(
        refused(add_songs(&bot, &bob, &id, &["s9"]).await),
        (StatusCode::FORBIDDEN, PRIVATE.to_owned())
    );

    assert_eq!(set(&bot, &bob, "s1", true).await.ok(), Some(true));
    assert_eq!(fake.lock().unwrap().playlists.len(), 2);
    assert_eq!(
        refused(set(&bot, &cy, "s1", true).await),
        (StatusCode::FORBIDDEN, NO_CERTIFICATE.to_owned())
    );
    assert!(!is_favorite(&bot, &cy, "s1").await);
}

#[tokio::test]
async fn removal_checks_the_song_edits_wait_their_turn_and_gone_playlists_are_pruned() {
    let _exclusive = API_TESTS.lock().await;
    let (server, fake) = library();
    let (directory, bot, _token) = test_bot(&server.url, 1);
    let [ada, ..] = people(&bot);
    let id = make(&bot, &ada, "Mix").await.ok().unwrap();
    add_songs(&bot, &ada, &id, &["a", "b", "c"])
        .await
        .ok()
        .unwrap();

    let before = server.requests().len();
    let stale = remove(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id, "index": 1, "song": "c"})),
    )
    .await;
    assert_eq!(refused(stale), (StatusCode::CONFLICT, STALE.to_owned()));
    assert_eq!(
        endpoints(&server)[before..],
        ["getPlaylist"],
        "re-read, nothing removed"
    );
    assert_eq!(songs(&fake, &id), ["a", "b", "c"]);
    remove(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id, "index": 1, "song": "b"})),
    )
    .await
    .ok()
    .unwrap();
    assert_eq!(songs(&fake, &id), ["a", "c"]);
    assert!(
        server
            .requests()
            .last()
            .unwrap()
            .contains("&songIndexToRemove=1 ")
    );

    // A change of the same playlist waits for the one in progress.
    let held = bot
        .playlists
        .lock(crate::playlists::playlist_lock(bot.libraries[0].key(), &id))
        .await
        .unwrap();
    let waiting = tokio::spawn({
        let (bot, ada, id) = (bot.clone(), ada.clone(), id.clone());
        async move { add_songs(&bot, &ada, &id, &["d"]).await.ok() }
    });
    let asked = server.requests().len();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.requests().len(), asked);
    drop(held);
    assert_eq!(waiting.await.unwrap(), Some(1));
    assert_eq!(songs(&fake, &id), ["a", "c", "d"]);

    // Deleted in the library: the next listing drops the owner entry, for good.
    fake.lock().unwrap().playlists.clear();
    let revision = bot.playlists.revision();
    assert!(listing(&bot, &ada).await.is_empty());
    assert_eq!(bot.playlists.get(bot.libraries[0].key(), &id), None);
    assert_eq!(bot.playlists.revision(), revision + 1);
    let reloaded = crate::playlists::Playlists::load(
        directory.path().join(crate::playlists::FILE),
        &[bot.libraries[0].key().to_owned()],
    )
    .unwrap();
    assert_eq!(reloaded.get(bot.libraries[0].key(), &id), None);
}

#[tokio::test]
async fn names_song_counts_and_playlists_per_person_are_bounded() {
    let _exclusive = API_TESTS.lock().await;
    let (server, fake) = library();
    let (_directory, bot, _token) = test_bot(&server.url, 1);
    let [ada, bob, _] = people(&bot);
    for name in ["", "   ", &"x".repeat(playlists::MAX_NAME_CHARS + 1)] {
        assert_eq!(
            refused(make(&bot, &ada, name).await).0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(server.requests().is_empty());

    let id = make(&bot, &ada, "Big").await.ok().unwrap();
    let many: Vec<String> = (0..=MAX_ADD).map(|index| format!("s{index}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        refused(add_songs(&bot, &ada, &id, &many).await).0,
        StatusCode::BAD_REQUEST
    );
    let long = "x".repeat(playlists::MAX_ID_BYTES + 1);
    for songs in [&[][..], &["ok", "a\nb"], &[long.as_str()]] {
        assert_eq!(
            refused(add_songs(&bot, &ada, &id, songs).await).0,
            StatusCode::BAD_REQUEST
        );
    }
    let both = add(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": id, "songs": ["s"], "album": "al"})),
    )
    .await;
    assert_eq!(refused(both).0, StatusCode::BAD_REQUEST);
    let asked = server.requests().len();
    assert_eq!(
        add_songs(&bot, &ada, &id, &many[..250]).await.ok(),
        Some(250)
    );
    let adds = server.requests()[asked..]
        .iter()
        .filter(|request| request.contains("songIdToAdd="))
        .count();
    assert_eq!(adds, 3, "100 songs per request");
    assert_eq!(songs(&fake, &id).len(), 250);

    fake.lock().unwrap().playlists[0].3 = vec!["s".into(); MAX_SONGS - 1];
    assert_eq!(
        refused(add_songs(&bot, &ada, &id, &["a", "b"]).await).0,
        StatusCode::CONFLICT
    );
    assert_eq!(add_songs(&bot, &ada, &id, &["a"]).await.ok(), Some(1));
    assert_eq!(
        refused(add_songs(&bot, &ada, &id, &["b"]).await).0,
        StatusCode::CONFLICT
    );

    for index in 1..playlists::MAX_PER_PERSON {
        make(&bot, &ada, &format!("List {index}"))
            .await
            .ok()
            .unwrap();
    }
    let (status, message) = refused(make(&bot, &ada, "One more").await);
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(message.contains("50 playlists"), "{message}");
    let favorites = set_favorite(
        State(bot.clone()),
        Path(ada.clone()),
        body(json!({"library": 0, "id": "s", "favorite": true})),
    )
    .await;
    assert_eq!(refused(favorites).0, StatusCode::CONFLICT);
    assert!(make(&bot, &bob, "Bob's").await.is_ok());
    assert_eq!(
        fake.lock().unwrap().playlists.len(),
        playlists::MAX_PER_PERSON + 1
    );
}
