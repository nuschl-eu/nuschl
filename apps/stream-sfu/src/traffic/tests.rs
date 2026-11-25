// SPDX-License-Identifier: EUPL-1.2
use super::*;

/// 2026-11-01, 2026-11-18, 2026-11-19 and 2026-12-01, 00:00 UTC.
const NOVEMBER: i64 = 1_793_491_200;
const NOVEMBER_18: i64 = 1_794_960_000;
const NOVEMBER_19: i64 = 1_795_046_400;
const DECEMBER: i64 = 1_796_083_200;

fn traffic(directory: &nuschl_testkit::TempDir, budget: u64, now: i64) -> Traffic {
    Traffic::load(directory.path().join("traffic"), budget, Period::Month, now).expect("load")
}

#[test]
fn periods_are_utc_months_and_days() {
    assert_eq!(Period::Month.bounds(NOVEMBER_18 + 5), (NOVEMBER, DECEMBER));
    assert_eq!(Period::Month.bounds(NOVEMBER), (NOVEMBER, DECEMBER));
    assert_eq!(
        Period::Day.bounds(NOVEMBER_18 + 5),
        (NOVEMBER_18, NOVEMBER_19)
    );
}

#[test]
fn allowance_falls_toward_the_remaining_period_average_as_bytes_are_spent() {
    let directory = nuschl_testkit::temp_dir();
    let middle = NOVEMBER + (DECEMBER - NOVEMBER) / 2;
    let budget = 20_000_000_000_000;
    let mut traffic = traffic(&directory, budget, middle);

    traffic.sent = 9_980_000_000_000;
    assert!(traffic.allowance_bps(middle) > 100_000_000);
    traffic.sent = 9_990_000_000_000;
    assert!((83_000_000..85_000_000).contains(&traffic.allowance_bps(middle)));
    traffic.sent = budget / 2;
    assert_eq!(traffic.allowance_bps(middle), 61_728_395);
    traffic.sent = 15_000_000_000_000;
    assert_eq!(traffic.allowance_bps(middle), 30_864_197);
    traffic.sent = budget;
    assert_eq!(traffic.allowance_bps(middle), 0);
    assert!(traffic.exhausted());
    assert!(traffic.allowance_bps(DECEMBER) > 59_000_000);
    assert_eq!(traffic.sent(), 0);
    assert!(!traffic.exhausted());
}

#[test]
fn accrued_monthly_share_covers_hours_of_above_average_traffic() {
    let directory = nuschl_testkit::temp_dir();
    let middle = NOVEMBER + (DECEMBER - NOVEMBER) / 2;
    let mut traffic = traffic(&directory, 20_000_000_000_000, middle);

    // Use banked traffic to send 100 Mbit/s for several hours, with a monthly
    // average of about 62 Mbit/s.
    for hour in 0..=6 {
        traffic.sent = 9_800_000_000_000 + 100_000_000 / 8 * 3_600 * hour;
        assert!(traffic.allowance_bps(middle + 3_600 * hour as i64) > 100_000_000);
    }
}

#[test]
fn the_reservation_survives_restart_and_stops_at_the_budget() {
    let directory = nuschl_testkit::temp_dir();
    let budget = RESERVATION_BYTES + 1_000;
    let mut before = traffic(&directory, budget, NOVEMBER_18);
    assert!(before.charge(700, NOVEMBER_18).expect("reserve"));
    assert_eq!(before.sent(), 700);
    let mut after = traffic(&directory, budget, NOVEMBER_19);
    assert_eq!(after.sent(), RESERVATION_BYTES);
    assert!(!after.charge(1_001, NOVEMBER_19).expect("over budget"));
    assert_eq!(after.sent(), RESERVATION_BYTES);
    assert!(after.charge(1_000, NOVEMBER_19).expect("exact budget"));
    assert!(!after.charge(1, NOVEMBER_19).expect("over budget"));
    assert_eq!(after.sent(), budget);
    assert!(after.charge(1, DECEMBER).expect("new budget"));
    assert_eq!(
        traffic(&directory, budget, DECEMBER).sent(),
        RESERVATION_BYTES,
        "the new period's reservation"
    );
}

#[test]
fn an_unwritable_budget_file_stops_sends_until_it_can_be_written() {
    let directory = nuschl_testkit::temp_dir();
    let state = directory.path().join("state");
    let mut traffic =
        Traffic::load(state.join("traffic"), 1_000, Period::Month, NOVEMBER).expect("load");
    assert!(traffic.charge(700, NOVEMBER).is_err());
    assert_eq!(traffic.sent(), 0);
    std::fs::create_dir(state).expect("create state directory");
    assert!(!traffic.charge(700, NOVEMBER).expect("wait to retry"));
    assert!(traffic.charge(700, NOVEMBER + 1).expect("reserve"));
}

#[test]
fn the_count_loads_from_the_existing_format_and_rejects_malformed_files() {
    let directory = nuschl_testkit::temp_dir();
    let path = directory.path().join("traffic");
    std::fs::write(&path, format!("{NOVEMBER} 700\n")).expect("write");
    assert_eq!(traffic(&directory, 1_000, NOVEMBER_19).sent(), 700);
    assert_eq!(traffic(&directory, 1_000, DECEMBER).sent(), 0);

    std::fs::write(&path, "garbage\n").expect("write");
    assert!(Traffic::load(path, 1_000, Period::Month, NOVEMBER_18).is_err());
}
