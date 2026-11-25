// SPDX-License-Identifier: EUPL-1.2
//! The egress budget per period (docs/video-plan.md §5.2, #443). The allowance
//! combines the sustainable rate for the rest of the period with an hourly draw
//! on unused traffic accrued so far. The SFU reserves the bytes on disk before
//! sending them and counts any outstanding reservation as spent after a restart.

use jiff::{Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use serde::Deserialize;
use std::{
    fs::File,
    io::{self, Write as _},
    path::PathBuf,
};

const RESERVATION_BYTES: u64 = 64 * 1024 * 1024;
const BURST_SECONDS: u128 = 60 * 60;

/// A billing period, in UTC.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Month,
}

impl Period {
    /// The period around `now`, in Unix seconds.
    fn bounds(self, now: i64) -> (i64, i64) {
        let date = Timestamp::from_second(now)
            .unwrap_or(Timestamp::UNIX_EPOCH)
            .to_zoned(TimeZone::UTC)
            .date();
        let start = match self {
            Self::Day => date,
            Self::Month => date.first_of_month(),
        };
        let end = match self {
            Self::Day => start.checked_add(1.day()),
            Self::Month => start.checked_add(1.month()),
        };
        let second = |date: Date| {
            date.to_zoned(TimeZone::UTC)
                .map(|z| z.timestamp().as_second())
        };
        match (second(start), end.map(second)) {
            (Ok(start), Ok(Ok(end))) => (start, end),
            _ => (now, now.saturating_add(86_400)),
        }
    }
}

pub struct Traffic {
    budget: u64,
    period: Period,
    path: PathBuf,
    start: i64,
    end: i64,
    sent: u64,
    reserved: u64,
    retry_at: Option<i64>,
}

impl Traffic {
    /// Reads the count of the current period from `path` (`start sent`), or
    /// starts at zero when the file is missing or from an earlier period.
    pub fn load(path: PathBuf, budget: u64, period: Period, now: i64) -> Result<Self, String> {
        let (start, end) = period.bounds(now);
        let sent = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut fields = text.split_whitespace();
                let saved = fields.next().and_then(|field| field.parse::<i64>().ok());
                let sent = fields.next().and_then(|field| field.parse::<u64>().ok());
                match (saved, sent, fields.next()) {
                    (Some(saved), Some(sent), None) => {
                        if saved == start {
                            sent
                        } else {
                            0
                        }
                    }
                    _ => return Err(format!("{}: unreadable traffic count", path.display())),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        Ok(Self {
            budget,
            period,
            path,
            start,
            end,
            sent,
            reserved: sent,
            retry_at: None,
        })
    }

    /// Reserves and counts an outgoing packet before the SFU sends it.
    pub fn charge(&mut self, bytes: usize, now: i64) -> io::Result<bool> {
        self.advance_period(now);
        let Some(next) = self.sent.checked_add(bytes as u64) else {
            return Ok(false);
        };
        if next > self.budget {
            return Ok(false);
        }
        if next > self.reserved {
            if self.retry_at.is_some_and(|retry_at| now < retry_at) {
                return Ok(false);
            }
            let reserved = self
                .sent
                .saturating_add(RESERVATION_BYTES)
                .max(next)
                .min(self.budget);
            if let Err(error) = self.persist_reservation(reserved) {
                self.retry_at = Some(now.saturating_add(1));
                return Err(error);
            }
            self.reserved = reserved;
            self.retry_at = None;
        }
        self.sent = next;
        Ok(true)
    }

    #[cfg(test)]
    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub fn exhausted(&self) -> bool {
        // Even an IPv4 UDP datagram needs 28 bytes of headers and a payload.
        self.budget.saturating_sub(self.sent) <= 28
    }

    /// Returns the aggregate allowance from the remaining budget and banked bytes.
    pub fn allowance_bps(&mut self, now: i64) -> u64 {
        self.advance_period(now);
        if self.exhausted() {
            return 0;
        }
        let duration = self.end.saturating_sub(self.start).max(1) as u128;
        let remaining_seconds = self.end.saturating_sub(now.max(self.start)).max(1) as u128;
        let paced = u128::from(self.budget) * (duration - remaining_seconds) / duration;
        let banked = paced.saturating_sub(u128::from(self.sent));
        let remaining_bytes = u128::from(self.budget.saturating_sub(self.sent));
        let allowance = remaining_bytes * 8 / remaining_seconds + banked * 8 / BURST_SECONDS;
        allowance.min(u128::from(u64::MAX)) as u64
    }

    /// A snapshot of charged bytes for operators. The reservation is the durable count.
    pub fn publish_usage(&self) -> io::Result<()> {
        let partial = self.path.with_extension("usage.partial");
        std::fs::write(&partial, format!("{} {}\n", self.start, self.sent))?;
        std::fs::rename(&partial, self.path.with_extension("usage"))
    }

    fn advance_period(&mut self, now: i64) {
        if now >= self.end {
            (self.start, self.end) = self.period.bounds(now);
            self.sent = 0;
            self.reserved = 0;
            self.retry_at = None;
        }
    }

    fn persist_reservation(&self, reserved: u64) -> io::Result<()> {
        let partial = self.path.with_extension("partial");
        let mut file = File::create(&partial)?;
        writeln!(file, "{} {reserved}", self.start)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&partial, &self.path)?;
        #[cfg(unix)]
        File::open(
            self.path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
