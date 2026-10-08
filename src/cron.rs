//! Cron: POSIX expressions, five fields in UTC, each naming a path that gets a `POST` with `Forwarded: for=_cron` in
//! every minute it matches. Local hosts tick in-process; on AWS each is an EventBridge Scheduler schedule, which
//! `deploy` and `release` keep in step with the release that runs.
use crate::aws::{Aws, Schedule, Target, Window};
use crate::serve::Tric;
use crate::state::{self, Install, hash};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use wasmtime::{Error, Result};

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

/// The values in `bits`, each plus `plus`, as a list.
fn list(bits: u64, plus: u32) -> String {
    (0..64).filter(|v| bits >> v & 1 == 1).map(|v| (v + plus).to_string()).collect::<Vec<_>>().join(",")
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
    pub fn parse(expr: &str) -> Result<Self, String> {
        let text: Vec<String> = expr.split_whitespace().map(str::to_owned).collect();
        let bad = || format!("bad cron expression {expr:?}: five fields of numbers, *, -, / and ,");
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

    /// As EventBridge Scheduler's cron expressions, which have a year, count weekdays from 1 for Sunday, step from a
    /// number, and restrict one day field, the other being `?`: two when both are restricted.
    pub fn eventbridge(&self) -> Vec<String> {
        let field = |i: usize| {
            let lo = FIELDS[i].0;
            let items = self.text[i].split(',').map(|item| match item.split_once('/') {
                Some(("*", step)) => format!("{lo}/{step}"),
                Some(_) => list(set(item, FIELDS[i]).unwrap_or_default(), 0),
                None => item.into(),
            });
            items.collect::<Vec<_>>().join(",")
        };
        let (min, hour, dom, month, dow) = (field(0), field(1), field(2), field(3), list(self.sets[4], 1));
        let cron = |dom: &str, dow: &str| format!("cron({min} {hour} {dom} {month} {dow} *)");
        match (self.restricted(2), self.restricted(4)) {
            (_, false) => vec![cron(&dom, "?")],
            (false, true) => vec![cron("?", &dow)],
            (true, true) => vec![cron(&dom, "?"), cron("?", &dow)],
        }
    }
}

/// Makes `app`'s schedules those of `cron`: creates the missing and deletes the rest. A schedule is named for the app
/// and for everything in it, so one that changes in any way is another schedule.
pub async fn sync(aws: &Aws, install: &Install, app: &str, cron: &BTreeMap<String, String>) -> Result<()> {
    let prefix = format!("{}-", &hash(app.as_bytes())[..16]);
    let mut want = BTreeMap::new();
    for (expr, path) in cron {
        let input = serde_json::json!({ "cron": { "app": app, "path": path } }).to_string();
        for schedule_expression in Cron::parse(expr).map_err(Error::msg)?.eventbridge() {
            let target = Target { arn: install.function.clone(), role_arn: install.role.clone(), input: input.clone() };
            let schedule = Schedule {
                schedule_expression,
                schedule_expression_timezone: "UTC",
                flexible_time_window: Window { mode: "OFF" },
                group_name: install.group.clone(),
                target,
            };
            want.insert(format!("{prefix}{}", &hash(&serde_json::to_vec(&schedule)?)[..16]), schedule);
        }
    }
    let have = aws.schedules(&install.group, &prefix).await?;
    for name in have.iter().filter(|n| !want.contains_key(*n)) {
        aws.delete(&install.group, name).await?;
    }
    for (name, schedule) in want.iter().filter(|(n, _)| !have.contains(n)) {
        aws.create(name, schedule).await?;
    }
    Ok(())
}

/// Fires every app's cron in-process, at the start of each minute, or only `tric`'s one app's.
pub async fn tick(tric: Arc<Tric>) {
    loop {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let next = (now / 60 + 1) * 60;
        sleep(Duration::from_secs(next - now)).await;
        if let Err(e) = fire(&tric, next).await {
            tracing::warn!("cron: {e:#}");
        }
    }
}

async fn fire(tric: &Arc<Tric>, t: u64) -> Result<()> {
    let apps = match tric.only() {
        Some(app) => vec![app.to_owned()],
        None => state::apps(&*tric.store).await?,
    };
    for app in apps {
        let Some(current) = state::current(&*tric.store, &app).await? else { continue };
        for (expr, path) in state::release(&*tric.store, &app, &current.release).await?.cron {
            if Cron::parse(&expr).is_ok_and(|c| c.matches(t)) {
                tokio::spawn(tric.clone().cron(app.clone(), path));
            }
        }
    }
    Ok(())
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

    #[test]
    fn eventbridge() {
        let e = |s: &str| Cron::parse(s).unwrap().eventbridge();
        assert_eq!(e("* * * * *"), ["cron(* * * * ? *)"]);
        assert_eq!(e("*/15 0-6/3 * * *"), ["cron(0/15 0,3,6 * * ? *)"]);
        assert_eq!(e("0 8 */2 * *"), ["cron(0 8 1/2 * ? *)"]);
        assert_eq!(e("0 8 * * 1-5"), ["cron(0 8 ? * 2,3,4,5,6 *)"]);
        assert_eq!(e("0 8 * * 0,7"), ["cron(0 8 ? * 1 *)"]);
        assert_eq!(e("0 8 1,15 * 1"), ["cron(0 8 1,15 * ? *)", "cron(0 8 ? * 2 *)"]);
    }
}
