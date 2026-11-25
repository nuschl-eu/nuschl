// SPDX-License-Identifier: EUPL-1.2
use super::*;

const ADMIT: Key = [1; 32];
const HOST: Key = [2; 32];
static ROOM: std::sync::LazyLock<RoomId> = std::sync::LazyLock::new(|| ([7; 16], verifier(&HOST)));

fn rooms() -> Rooms {
    Rooms::new(Limits {
        rooms: 2,
        peers_per_room: 3,
        lifetime: HOUR,
        lease: HOUR,
        creations_per_hour: 2,
        room_bitrate: 1_000_000,
    })
}

fn join(nonce: &Nonce, host: bool, create: bool) -> Join {
    Join {
        room: *ROOM,
        proof: prove(&ADMIT, ADMIT_LABEL, nonce),
        host_proof: host.then(|| prove(&HOST, HOST_LABEL, nonce)),
        create: create.then_some((ADMIT, HOST)),
        decoders: crate::protocol::Decoders::default(),
        video_limits: false,
        screen_rate: None,
    }
}

fn client() -> IpAddr {
    "192.0.2.1".parse().expect("ip")
}

#[test]
fn admission_needs_proofs_bound_to_the_nonce() {
    let now = Instant::now();
    let mut rooms = rooms();
    let n = [9; 32];
    assert_eq!(
        rooms.join(1, &join(&n, false, false), &n, client(), now),
        Err(Refusal::NoRoom)
    );
    // Creating needs P.
    assert_eq!(
        rooms.join(1, &join(&n, false, true), &n, client(), now),
        Err(Refusal::Proof)
    );
    assert_eq!(
        rooms.join(1, &join(&n, true, true), &n, client(), now),
        Ok(Role::Host)
    );
    // A proof made for another nonce fails.
    let other = [8; 32];
    assert_eq!(
        rooms.join(2, &join(&other, false, false), &n, client(), now),
        Err(Refusal::Proof)
    );
    let mut bad_host = join(&n, false, false);
    bad_host.host_proof = Some(prove(&ADMIT, HOST_LABEL, &n));
    assert_eq!(
        rooms.join(2, &bad_host, &n, client(), now),
        Err(Refusal::Proof)
    );
    assert_eq!(
        rooms.join(2, &join(&n, false, false), &n, client(), now),
        Ok(Role::Participant)
    );
    // The same keys join; other keys for the same room id are refused.
    assert_eq!(
        rooms.join(3, &join(&n, true, true), &n, client(), now),
        Ok(Role::Host)
    );
    let mut squat = join(&n, true, true);
    squat.create = Some(([3; 32], [4; 32]));
    squat.proof = prove(&[3; 32], ADMIT_LABEL, &n);
    squat.host_proof = Some(prove(&[4; 32], HOST_LABEL, &n));
    assert_eq!(
        rooms.join(4, &squat, &n, client(), now),
        Err(Refusal::Proof)
    );
    // Keys of its own make another room: the link's V names the first one.
    squat.room.1 = verifier(&[4; 32]);
    assert_eq!(rooms.join(4, &squat, &n, client(), now), Ok(Role::Host));
    assert_eq!(rooms.get(&ROOM).expect("room").peers.len(), 3);
    // Creating needs a host_key that hashes to V.
    let mut empty = self::rooms();
    let mut wrong_v = join(&n, true, true);
    wrong_v.room.1 = verifier(&[5; 32]);
    assert_eq!(
        empty.join(5, &wrong_v, &n, client(), now),
        Err(Refusal::Proof)
    );
}

#[test]
fn verifier_matches_the_link_vector() {
    // host_key from P = 00 01 … 1f (protocol.rs).
    let host_key = hex::decode("714cce84239301bcc3f4fcaaf0d59ca0d242f7b0bc9cf276ab9a3568bf9a873f")
        .expect("hex");
    assert_eq!(
        hex::encode(verifier(&host_key.try_into().expect("32 bytes"))),
        "078384c474855fcaa97ecbb155d67f03add025efa0c126d8a23c30e71d6c4c37"
    );
}

#[test]
fn bookkeeping_and_bounds() {
    let now = Instant::now();
    let mut rooms = rooms();
    let n = [9; 32];
    for peer in 1..=3 {
        rooms
            .join(peer, &join(&n, peer == 1, peer == 1), &n, client(), now)
            .expect("join");
    }
    assert_eq!(
        rooms.join(4, &join(&n, false, false), &n, client(), now),
        Err(Refusal::Full)
    );
    rooms.leave(&ROOM, 3);
    assert_eq!(
        rooms.join(4, &join(&n, false, false), &n, client(), now),
        Ok(Role::Participant)
    );

    // Only a host ends the room.
    assert!(rooms.is_host(&ROOM, 1) && !rooms.is_host(&ROOM, 4) && !rooms.is_host(&ROOM, 9));
    assert_eq!(rooms.end(&ROOM, 4), Err(Refusal::NotAllowed));
    let mut ended = rooms.end(&ROOM, 1).expect("end");
    ended.sort();
    assert_eq!(ended, vec![1, 2, 4]);
    assert_eq!(
        rooms.join(5, &join(&n, true, true), &n, client(), now),
        Err(Refusal::Ended)
    );

    // Empty rooms keep their keys until they expire.
    assert!(rooms.expire(now + HOUR / 2).is_empty());
    assert!(rooms.get(&ROOM).is_some());
    assert!(rooms.expire(now + HOUR).is_empty());
    assert!(rooms.get(&ROOM).is_none());
}

#[test]
fn expiry_disconnects_peers() {
    let now = Instant::now();
    let mut rooms = rooms();
    let n = [9; 32];
    rooms
        .join(1, &join(&n, true, true), &n, client(), now)
        .expect("join");
    rooms
        .join(2, &join(&n, false, false), &n, client(), now)
        .expect("join");
    let mut expired = rooms.expire(now + HOUR);
    expired.sort();
    assert_eq!(expired, vec![1, 2]);
    assert_eq!(
        rooms.join(3, &join(&n, false, false), &n, client(), now),
        Err(Refusal::NoRoom)
    );
}

#[test]
fn room_count_and_creation_rate() {
    let now = Instant::now();
    let mut rooms = Rooms::new(Limits {
        rooms: 3,
        ..rooms().limits
    });
    let n = [9; 32];
    let create = |room: u8| Join {
        room: ([room; 16], verifier(&HOST)),
        ..join(&n, true, true)
    };
    let ip = |text: &str| text.parse::<IpAddr>().expect("ip");
    // Two creations per /64 and hour.
    rooms
        .join(1, &create(1), &n, ip("2001:db8::1"), now)
        .expect("first");
    rooms
        .join(2, &create(2), &n, ip("2001:db8::2"), now)
        .expect("second");
    assert_eq!(
        rooms.join(3, &create(3), &n, ip("2001:db8::3"), now),
        Err(Refusal::Rate)
    );
    rooms
        .join(3, &create(3), &n, ip("2001:db8:0:1::1"), now)
        .expect("other /64");
    // The room table is full.
    assert_eq!(
        rooms.join(4, &create(4), &n, ip("192.0.2.9"), now),
        Err(Refusal::Rooms)
    );
    assert_eq!(rooms.expire(now + HOUR).len(), 3);
    rooms
        .join(5, &create(5), &n, ip("2001:db8::ffff"), now + HOUR)
        .expect("next hour");
}

#[test]
fn leases() {
    let now = Instant::now();
    let mut rooms = Rooms::new(Limits {
        lease: Duration::from_secs(30),
        ..rooms().limits
    });
    let n = [9; 32];
    let second = Duration::from_secs(1);
    rooms
        .join(1, &join(&n, true, true), &n, client(), now)
        .expect("join");
    assert_eq!(
        rooms.lease(&ROOM, &[4; 32], false, now),
        Err(Refusal::Proof)
    );
    let other = ([8; 16], ROOM.1);
    assert_eq!(rooms.lease(&other, &HOST, false, now), Err(Refusal::NoRoom));
    assert_eq!(rooms.lease(&ROOM, &HOST, false, now), Ok(vec![]));
    assert_eq!(
        rooms.lease(&ROOM, &HOST, false, now + second / 2),
        Err(Refusal::Rate)
    );
    assert_eq!(
        rooms.lease(&ROOM, &HOST, false, now + 20 * second),
        Ok(vec![])
    );
    // Renewed at 20 s, so the room lasts to 50 s.
    assert!(rooms.expire(now + 49 * second).is_empty());
    assert_eq!(rooms.expire(now + 50 * second), vec![1]);
    assert_eq!(
        rooms.join(2, &join(&n, false, false), &n, client(), now),
        Err(Refusal::Ended)
    );
    assert_eq!(
        rooms.lease(&ROOM, &HOST, false, now + 51 * second),
        Err(Refusal::NoRoom)
    );

    // Ending through the lease.
    let room = ([6; 16], ROOM.1);
    let create = Join {
        room,
        ..join(&n, true, true)
    };
    rooms.join(3, &create, &n, client(), now).expect("join");
    assert_eq!(rooms.lease(&room, &HOST, true, now), Ok(vec![3]));
    assert_eq!(
        rooms.join(4, &create, &n, client(), now),
        Err(Refusal::Ended)
    );
}

#[test]
fn buckets() {
    let now = Instant::now();
    let mut bucket = Bucket::new(10.0, 20.0, now);
    assert!(bucket.take(20.0, now));
    assert!(!bucket.take(1.0, now));
    assert!(bucket.take(10.0, now + Duration::from_secs(1)));
    assert!(
        !bucket.take(25.0, now + Duration::from_secs(100)),
        "capped at the burst"
    );
    // A credit is paid back before the next take, and reaches one burst.
    let later = now + Duration::from_secs(200);
    assert!(bucket.lend(25.0, later));
    assert!(!bucket.take(1.0, later));
    assert!(bucket.take(1.0, later + Duration::from_millis(600)));
    assert!(bucket.lend(20.0, later + Duration::from_millis(600)));
    assert!(!bucket.lend(10.0, later + Duration::from_millis(600)));
}

#[test]
fn turn_credentials_follow_the_rest_scheme() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let (username, password) = turn_credentials(b"secret", Duration::from_secs(600), now);
    let (expiry, user) = username.split_once(':').expect("expiry:user");
    assert_eq!(expiry, "1000600");
    assert_eq!(user.len(), 16);
    use base64::Engine as _;
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(b"secret").expect("key");
    mac.update(username.as_bytes());
    let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    assert_eq!(password, expected);
    assert_ne!(
        turn_credentials(b"secret", Duration::from_secs(600), now).0,
        username
    );
}
