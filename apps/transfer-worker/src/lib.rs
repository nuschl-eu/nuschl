// SPDX-License-Identifier: EUPL-1.2
//! The parts of the transfer worker that decide what it does, apart from the
//! magic-wormhole calls that do it: which transit abilities a transfer may
//! use, which relay hints it passes on, how long a failed code waits, how
//! often progress leaves, and how far a download may go. `main.rs` keeps the
//! wiring and the paths that are one event per step.
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};

use magic_wormhole::{transfer, transit};
use nuschl_ipc::WormholeServers;

/// Pickups of one offer that run at once; the next code waits for a free slot.
pub const MAX_PICKUPS: usize = 4;
/// Received bytes per `Data` event.
pub const CHUNK: usize = 256 * 1024;
/// Words in a code ("7-guitarist-revenge"), as other wormhole clients make them.
pub const CODE_WORDS: usize = 2;

/// How long the offer waits before its next code, after `failures` failures in
/// a row: ten seconds for the first, doubling each time, at most about five
/// minutes. A caller with no failure asks for no wait.
pub fn retry_delay(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs(5_u64 << failures.min(6))
}

/// What a transfer may do on the network. Without `direct` the connection goes
/// through the configured relay only: no direct sockets, and none of the
/// relays the other side names (SECURITY.md).
pub fn abilities(servers: &WormholeServers) -> transit::Abilities {
    if servers.direct {
        transit::Abilities::ALL
    } else {
        transit::Abilities::FORCE_RELAY
    }
}

/// The transit relay of this transfer: the one `servers` names, never one the
/// other side suggests. A malformed address fails before any connection.
pub fn relay_hints(servers: &WormholeServers) -> Result<Vec<transit::RelayHint>, String> {
    let url = servers
        .relay
        .parse()
        .map_err(|_| "The transit relay address is invalid.")?;
    let hint = transit::RelayHint::from_urls(None, [url]).map_err(|error| error.to_string())?;
    Ok(vec![hint])
}

/// The magic-wormhole configuration: the rendezvous server this transfer uses.
pub fn config(servers: &WormholeServers) -> magic_wormhole::AppConfig<transfer::AppVersion> {
    transfer::APP_CONFIG.rendezvous_url(Cow::Owned(servers.rendezvous.clone()))
}

/// Progress leaves about four times a second, and always at the end.
#[derive(Default)]
pub struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    /// Whether progress `done` of `total` goes out at `now`: the end always
    /// does, the rest at most every 250 ms.
    pub fn admit(&mut self, done: u64, total: u64, now: Instant) -> bool {
        let due = done == total
            || self.last.is_none_or(|last| {
                now.saturating_duration_since(last) >= Duration::from_millis(250)
            });
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// The bytes a download may still take: exactly what the other side announced,
/// so a straggling write cannot run past it.
#[derive(Debug)]
pub struct Budget {
    remaining: u64,
}

impl Budget {
    pub fn new(announced: u64) -> Self {
        Self {
            remaining: announced,
        }
    }

    /// Count `bytes` against the announced size.
    pub fn take(&mut self, bytes: usize) -> Result<(), &'static str> {
        let Some(remaining) = self.remaining.checked_sub(bytes as u64) else {
            return Err("more data than the offer announced");
        };
        self.remaining = remaining;
        Ok(())
    }

    /// Whether everything announced has arrived.
    pub fn complete(&self) -> bool {
        self.remaining == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{Budget, Throttle, abilities, relay_hints, retry_delay};
    use nuschl_ipc::WormholeServers;
    use std::time::{Duration, Instant};

    fn servers(direct: bool) -> WormholeServers {
        WormholeServers {
            rendezvous: "ws://127.0.0.1:4000/v1".into(),
            relay: "tcp://127.0.0.1:4001".into(),
            direct,
        }
    }

    #[test]
    fn a_relay_only_transfer_uses_the_configured_relay_and_no_direct_connection() {
        let relayed = servers(false);
        let relay_only = abilities(&relayed);
        assert!(
            !relay_only.can_direct(),
            "a relay-only transfer may not open a direct connection"
        );
        assert!(relay_only.can_relay());
        let hints = relay_hints(&relayed).unwrap();
        assert_eq!(hints.len(), 1, "the configured relay is the only hint");
        let hint = &hints[0];
        assert_eq!(
            hint.tcp
                .iter()
                .map(|direct| (direct.hostname.as_str(), direct.port))
                .collect::<Vec<_>>(),
            [("127.0.0.1", 4001)],
            "the hint names the relay of the offer, not one of the peer's"
        );
        // A direct transfer may also use what the other side offers.
        let direct = abilities(&servers(true));
        assert!(direct.can_direct() && direct.can_relay());
    }

    #[test]
    fn a_malformed_relay_address_fails_before_any_transfer() {
        let broken = WormholeServers {
            relay: "not a url".into(),
            ..servers(true)
        };
        assert_eq!(
            relay_hints(&broken).unwrap_err(),
            "The transit relay address is invalid."
        );
    }

    #[test]
    fn a_failed_code_waits_longer_each_time_and_then_stops_growing() {
        assert_eq!(retry_delay(0), Duration::ZERO);
        assert_eq!(retry_delay(1), Duration::from_secs(10));
        assert_eq!(retry_delay(2), Duration::from_secs(20));
        assert_eq!(retry_delay(6), Duration::from_secs(320));
        assert_eq!(retry_delay(7), Duration::from_secs(320));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(320));
    }

    #[test]
    fn progress_goes_out_at_once_then_at_most_every_quarter_second() {
        let start = Instant::now();
        let mut throttle = Throttle::default();
        assert!(throttle.admit(0, 1000, start), "the first one goes");
        assert!(!throttle.admit(10, 1000, start + Duration::from_millis(249)));
        assert!(throttle.admit(10, 1000, start + Duration::from_millis(250)));
        assert!(!throttle.admit(10, 1000, start + Duration::from_millis(300)));
        assert!(
            throttle.admit(1000, 1000, start + Duration::from_millis(310)),
            "the end always goes"
        );
    }

    #[test]
    fn a_download_takes_exactly_what_its_offer_announced() {
        let mut budget = Budget::new(10);
        assert!(budget.take(6).is_ok());
        assert!(!budget.complete());
        assert!(budget.take(4).is_ok());
        assert!(budget.complete());
        assert_eq!(
            budget.take(1).unwrap_err(),
            "more data than the offer announced"
        );
        assert!(
            Budget::new(u64::MAX).take(usize::MAX).is_ok(),
            "a read cannot overflow the announced size"
        );
    }
}
