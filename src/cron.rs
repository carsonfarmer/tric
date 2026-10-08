//! Cron: POSIX expressions, five fields in UTC, each naming a path that gets a `POST` with `Forwarded: for=_cron` in
//! every minute it matches.
use crate::tric::Tric;
use http_body_util::BodyExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use wasmtime::{Result, format_err};

/// Each field's range: minute, hour, day of the month, month, day of the week (Sunday is 0 and 7).
const FIELDS: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];

/// An expression: each field as the values it matches, as bits, and as written.
pub struct Cron {
    sets: [u64; 5],
    text: Vec<String>,
}

/// The values of `field`, a list of `*`, `N` or `N-M`, each but `N` with an optional `/step`, as bits.
fn set(field: &str, (lo, hi): (u32, u32)) -> Option<u64> {
    let num = |s: &str| s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse::<u32>().ok()).flatten();
    let mut bits = 0;
    for item in field.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, num(step).filter(|s| *s > 0)?),
            None => (item, 1),
        };
        let (a, b) = match range.split_once('-') {
            _ if range == "*" => (lo, hi),
            Some((a, b)) => (num(a)?, num(b)?),
            None if step == 1 => (num(range)?, num(range)?),
            None => return None,
        };
        if a < lo || b > hi || a > b {
            return None;
        }
        bits |= (a..=b).step_by(step as usize).fold(0, |m, v| m | 1 << v);
    }
    Some(bits)
}

/// The year, month and day of a day since 1970-01-01, in the proleptic Gregorian calendar (Howard Hinnant's algorithm).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    (yoe + era * 400 + u64::from(m <= 2), m, d)
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self> {
        let text: Vec<String> = expr.split_whitespace().map(str::to_owned).collect();
        let bad = || format_err!("bad cron expression {expr:?}: five fields of numbers, *, -, / and ,");
        if text.len() != 5 {
            return Err(bad());
        }
        let mut sets = [0; 5];
        for (i, field) in text.iter().enumerate() {
            sets[i] = set(field, FIELDS[i]).ok_or_else(bad)?;
        }
        sets[4] = (sets[4] | sets[4] >> 7) & 0x7f; // 7 is Sunday
        Ok(Self { sets, text })
    }

    /// Whether a day field is restricted: anything but `*`. When both are, a day matches if either does.
    fn restricted(&self, i: usize) -> bool {
        self.text[i] != "*"
    }

    /// Whether it fires in the minute that holds the Unix second `t`.
    pub fn matches(&self, t: u64) -> bool {
        let days = t / 86_400;
        let (_, month, day) = civil(days);
        let has = |i: usize, v: u64| self.sets[i] >> v & 1 == 1;
        let (dom, dow) = (has(2, day), has(4, (days + 4) % 7)); // 1970-01-01 was a Thursday
        let day = if self.restricted(2) && self.restricted(4) { dom || dow } else { dom && dow };
        has(0, t / 60 % 60) && has(1, t / 3600 % 24) && has(3, month) && day
    }
}

/// Fires `jobs`, each an expression and the URL it posts to, at the start of each minute.
pub async fn tick(tric: Arc<Tric>, jobs: Vec<(Cron, String)>) {
    loop {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let next = (now / 60 + 1) * 60;
        sleep(Duration::from_secs(next - now)).await;
        for (_, url) in jobs.iter().filter(|(cron, _)| cron.matches(next)) {
            tokio::spawn(fire(tric.clone(), url.clone()));
        }
    }
}

/// Posts to `url`, as cron.
pub async fn fire(tric: Arc<Tric>, url: String) {
    let res = match http::Request::post(&url).body(Default::default()) {
        Ok(req) => tric.fetch(req, "for=_cron").await,
        Err(e) => return tracing::warn!(url, "cron: {e}"),
    };
    let status = res.status();
    _ = res.into_body().collect().await;
    tracing::info!(url, status = status.as_u16(), "cron");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-08, a Thursday, at 12:34 UTC.
    const T: u64 = 1_791_462_840;

    #[test]
    fn calendar() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(T / 86_400), (2026, 10, 8));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!((T / 86_400 + 4) % 7, 4);
    }

    #[test]
    fn matching() {
        let m = |e: &str| Cron::parse(e).unwrap().matches(T);
        for yes in [
            "* * * * *",
            "34 12 * * *",
            "*/2 * * * *",
            "30-40/2 * 8 10 *",
            "34 12 * * 4",
            "34 12 1 * 4",
            "0,34 * * * *",
        ] {
            assert!(m(yes), "{yes}");
        }
        for no in [
            "35 * * * *",
            "*/5 * * * *",
            "34 13 * * *",
            "* * 9 * *",
            "* * * 11 *",
            "* * * * 5",
            "* * 9 * 5",
            "* * */2 * *",
        ] {
            assert!(!m(no), "{no}");
        }
        assert!(m("* * 1 * 4"), "either day field, when both are restricted");
        assert!(m("* * 8 * 0"));
        assert!(Cron::parse("* * * * 7").unwrap().matches(T + 3 * 86_400), "7 is Sunday");
        for bad in ["", "* * * *", "* * * * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "* * * 13 *", "* * * * 8"] {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
        for bad in ["5/2 * * * *", "*/0 * * * *", "a * * * *", "1- * * * *", "5-1 * * * *", "-1 * * * *", "* * * JAN *"]
        {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
    }
}
