// SPDX-License-Identifier: EUPL-1.2
use super::*;

fn entry(library: &str, id: &str, owner: &str) -> Entry {
    Entry {
        library: library.into(),
        id: id.into(),
        owner: owner.into(),
        owner_name: format!("name of {owner}"),
        shared: false,
        favorites: false,
    }
}

fn caller(pseudonym: Option<&str>) -> Caller {
    Caller {
        pseudonym: pseudonym.map(str::to_owned),
        name: "Caller".into(),
    }
}

#[test]
fn ownership_survives_a_restart_and_leaves_with_its_library() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(FILE);
    let keys = ["a".repeat(32), "b".repeat(32)];
    let playlists = Playlists::load(path.clone(), &keys).unwrap();
    assert!(!path.exists(), "nothing to save yet");
    playlists.insert(entry(&keys[0], "1", "ra")).unwrap();
    playlists
        .insert(Entry {
            favorites: true,
            ..entry(&keys[1], "2", "ra")
        })
        .unwrap();
    playlists.set_shared(&keys[0], "1", true).unwrap();
    assert!(playlists.rename_owner("ra", "Ada").unwrap());
    assert!(!playlists.rename_owner("ra", "Ada").unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let again = Playlists::load(path.clone(), &keys).unwrap();
    let first = again.get(&keys[0], "1").unwrap();
    assert!(first.shared && !first.favorites && first.owner_name == "Ada");
    assert_eq!(again.favorites(&keys[1], "ra").unwrap().id, "2");
    assert_eq!(again.favorites(&keys[0], "ra"), None);

    // The second library's key left library_keys.json: its entries go for good.
    let fewer = Playlists::load(path.clone(), &keys[..1]).unwrap();
    assert_eq!(fewer.get(&keys[1], "2"), None);
    let reloaded = Playlists::load(path.clone(), &keys).unwrap();
    assert_eq!(reloaded.get(&keys[1], "2"), None);
    assert!(reloaded.get(&keys[0], "1").is_some());

    std::fs::write(&path, br#"{"version":2,"playlists":[]}"#).unwrap();
    assert!(Playlists::load(path.clone(), &keys).is_err());
    std::fs::write(&path, b"[]").unwrap();
    assert!(Playlists::load(path, &keys).is_err());
}

#[test]
fn owners_manage_shared_playlists_take_edits_and_certificates_are_required() {
    let owned = entry("k", "1", "ra");
    let shared = Entry {
        shared: true,
        ..owned.clone()
    };
    let favorites = Entry {
        favorites: true,
        ..owned.clone()
    };
    let (owner, other, anonymous) = (caller(Some("ra")), caller(Some("rb")), caller(None));

    let mine = access(Some(&owned), &owner);
    assert!(mine.personal && mine.mine && mine.editable && mine.manageable);
    assert_eq!(mine.owner_name.as_deref(), Some("name of ra"));
    assert_eq!(
        access(Some(&owned), &other),
        Access {
            personal: true,
            owner_name: Some("name of ra".into()),
            ..Access::default()
        }
    );
    let edits = access(Some(&shared), &other);
    assert!(edits.editable && !edits.manageable && !edits.mine && edits.shared);
    for entry in [&owned, &shared, &favorites] {
        let seen = access(Some(entry), &anonymous);
        assert!(!seen.editable && !seen.manageable && !seen.mine);
    }
    let favorite = access(Some(&favorites), &owner);
    assert!(favorite.favorites && favorite.editable && !favorite.manageable);
    assert!(!access(Some(&favorites), &other).editable);
    assert_eq!(access(None, &owner), Access::default());
    assert!(!anonymous.can_own() && owner.can_own());
}

#[test]
fn only_people_with_a_certificate_get_a_pseudonym() {
    let directory = tempfile::tempdir().unwrap();
    let pseudonyms = Pseudonyms::load(directory.path()).unwrap();
    let person = |certificate: Option<&str>| Owner {
        server: "voice.example:64738".into(),
        name: "x".repeat(200),
        certificate: certificate.map(str::to_owned),
    };
    let with = Caller::new(&person(Some("aa11")), &pseudonyms);
    assert_eq!(
        with.pseudonym.as_deref(),
        Some(pseudonyms.of(&person(Some("aa11"))).as_str())
    );
    assert_eq!(with.name.chars().count(), MAX_OWNER_NAME_CHARS);
    for certificate in [None, Some("")] {
        assert!(!can_own(&person(certificate)));
        assert_eq!(
            Caller::new(&person(certificate), &pseudonyms).pseudonym,
            None
        );
    }
}

#[test]
fn names_ids_and_caps_are_bounded() {
    assert_eq!(valid_name("  Road trip \n").as_deref(), Some("Road trip"));
    assert_eq!(
        valid_name(&"é".repeat(MAX_NAME_CHARS))
            .unwrap()
            .chars()
            .count(),
        100
    );
    for invalid in ["", "   ", "tab\there", "bell\u{7}"] {
        assert_eq!(valid_name(invalid), None, "{invalid:?}");
    }
    assert_eq!(valid_name(&"x".repeat(MAX_NAME_CHARS + 1)), None);
    assert!(valid_song_id("song-1") && valid_song_id(&"s".repeat(MAX_ID_BYTES)));
    assert!(!valid_song_id("") && !valid_song_id(&"s".repeat(MAX_ID_BYTES + 1)));
    assert!(!valid_song_id("a\nb"));
    assert_eq!(favorites_name("Ada"), "Favorites of Ada");
    assert_eq!(favorites_name(&"x".repeat(120)), "Favorites");

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(FILE);
    let playlists = Playlists::load(path.clone(), &["k".into()]).unwrap();
    for index in 0..MAX_PER_PERSON {
        playlists
            .insert(entry("k", &index.to_string(), "ra"))
            .unwrap();
    }
    assert!(matches!(playlists.room_for("ra"), Err(Full::Person)));
    assert!(matches!(
        playlists.insert(entry("k", "extra", "ra")),
        Err(Full::Person)
    ));
    assert!(playlists.room_for("rb").is_ok());

    let everyone: Vec<_> = (0..MAX_ENTRIES)
        .map(|index| entry("k", &index.to_string(), &format!("r{index}")))
        .collect();
    let bytes = serde_json::to_vec(&Saving {
        version: VERSION,
        playlists: &everyone,
    })
    .unwrap();
    std::fs::write(&path, bytes).unwrap();
    let full = Playlists::load(path.clone(), &["k".into()]).unwrap();
    assert!(matches!(full.room_for("new"), Err(Full::Everyone)));
    assert!(matches!(
        full.insert(entry("k", "new", "new")),
        Err(Full::Everyone)
    ));
    let too_many: Vec<_> = (0..=MAX_ENTRIES)
        .map(|index| entry("k", &index.to_string(), "r"))
        .collect();
    let bytes = serde_json::to_vec(&Saving {
        version: VERSION,
        playlists: &too_many,
    })
    .unwrap();
    std::fs::write(&path, bytes).unwrap();
    assert!(Playlists::load(path, &["k".into()]).is_err());
}

#[test]
fn pruning_drops_only_entries_known_before_the_list_and_missing_from_it() {
    let directory = tempfile::tempdir().unwrap();
    let playlists =
        Playlists::load(directory.path().join(FILE), &["k".into(), "other".into()]).unwrap();
    for id in ["kept", "gone"] {
        playlists.insert(entry("k", id, "ra")).unwrap();
    }
    playlists.insert(entry("other", "gone", "ra")).unwrap();
    let before = playlists.ids("k");
    // Made while the library listed its playlists.
    playlists.insert(entry("k", "new", "ra")).unwrap();
    assert_eq!(
        playlists
            .prune("k", &before, ["kept", "elsewhere"])
            .unwrap(),
        1
    );
    assert!(playlists.get("k", "kept").is_some());
    assert!(playlists.get("k", "new").is_some());
    assert_eq!(playlists.get("k", "gone"), None);
    assert!(playlists.get("other", "gone").is_some());
    let again =
        Playlists::load(directory.path().join(FILE), &["k".into(), "other".into()]).unwrap();
    assert_eq!(again.get("k", "gone"), None);
}

#[tokio::test]
async fn changes_of_one_key_wait_for_each_other_and_idle_locks_go() {
    let directory = tempfile::tempdir().unwrap();
    let playlists = Arc::new(Playlists::load(directory.path().join(FILE), &[]).unwrap());
    let first = playlists.lock(playlist_lock("k", "1")).await.unwrap();
    let other = playlists.lock(playlist_lock("k", "2")).await.unwrap();
    let person = playlists.lock(person_lock("ra")).await.unwrap();
    let waiting = tokio::spawn({
        let playlists = playlists.clone();
        async move { playlists.lock(playlist_lock("k", "1")).await.is_some() }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished());
    drop(first);
    assert!(waiting.await.unwrap());
    drop((other, person));
    let _again = playlists.lock(person_lock("rb")).await.unwrap();
    assert_eq!(playlists.locks.lock().unwrap().len(), 1);
}
