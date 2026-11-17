//! Daily rescan schedule (`ORG3D_RESCAN_AT=HH:MM`, in the container's `TZ`).

use anyhow::{Context, Result};
use jiff::{ZonedDateTime as Zoned, civil::Time};

pub fn parse_time(s: &str) -> Result<Time> {
    s.trim()
        .parse::<Time>()
        .with_context(|| format!("invalid rescan time {s:?}, expected HH:MM (24h)"))
}

/// The first occurrence of `at` strictly after `now`, in `now`'s time zone.
/// DST gaps/folds are resolved by jiff's default (compatible) disambiguation.
pub fn next_run(now: &Zoned, at: Time) -> Result<Zoned> {
    let mut date = now.date();
    loop {
        let candidate = date
            .to_datetime(at)
            .to_zoned_datetime(now.time_zone().clone())?;
        if candidate > *now {
            return Ok(candidate);
        }
        date = date.tomorrow()?;
    }
}

/// How long to sleep until the next run.
pub fn until_next(at: Time) -> Result<(Zoned, std::time::Duration)> {
    let now = Zoned::now();
    let next = next_run(&now, at)?;
    let wait = std::time::Duration::try_from(next.duration_since(&now))
        .unwrap_or(std::time::Duration::from_secs(60));
    Ok((next, wait))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hh_mm() {
        assert_eq!(parse_time("03:30").unwrap(), Time::constant(3, 30, 0, 0));
        assert!(parse_time("25:00").is_err());
        assert!(parse_time("soon").is_err());
    }

    #[test]
    fn next_run_later_today_or_tomorrow() {
        let now: Zoned = "2026-11-16T02:00:00[America/Los_Angeles]".parse().unwrap();
        let at = Time::constant(3, 0, 0, 0);
        assert_eq!(
            next_run(&now, at).unwrap().to_string(),
            "2026-11-16T03:00:00-08:00[America/Los_Angeles]"
        );
        let now: Zoned = "2026-11-16T03:00:00[America/Los_Angeles]".parse().unwrap();
        assert_eq!(
            next_run(&now, at).unwrap().to_string(),
            "2026-11-17T03:00:00-08:00[America/Los_Angeles]"
        );
    }
}
