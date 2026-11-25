// SPDX-License-Identifier: AGPL-3.0-only

//! 5-field cron expressions (minute hour day-of-month month day-of-week)
//! with lists, ranges, steps, month/day names and the @hourly/@daily/
//! @weekly/@monthly/@yearly macros. As in Vixie cron, when both day fields
//! are restricted a day matches if either does.

use jiff::civil::DateTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u32,
    dom: u32,
    month: u16,
    dow: u8,
    dom_any: bool,
    dow_any: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn value(s: &str, lo: u32, hi: u32, names: &[&str], base: u32) -> Result<u32, String> {
    let lower = s.to_ascii_lowercase();
    if let Some(i) = names.iter().position(|n| *n == lower) {
        return Ok(i as u32 + base);
    }
    let v: u32 = s.parse().map_err(|_| format!("bad value {s:?}"))?;
    if v < lo || v > hi {
        return Err(format!("{v} is outside {lo}-{hi}"));
    }
    Ok(v)
}

/// Bits lo..=hi set by one field; `true` when the field is `*`.
fn field(s: &str, lo: u32, hi: u32, names: &[&str], base: u32) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => {
                let st: u32 = st.parse().map_err(|_| format!("bad step in {part:?}"))?;
                if st == 0 {
                    return Err(format!("step 0 in {part:?}"));
                }
                (r, st)
            }
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                value(a, lo, hi, names, base)?,
                value(b, lo, hi, names, base)?,
            )
        } else {
            let v = value(range, lo, hi, names, base)?;
            (v, if part.contains('/') { hi } else { v })
        };
        if a > b {
            return Err(format!("empty range {range:?}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, s == "*"))
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self, String> {
        let e = expr.trim();
        let e = match e {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            e => e,
        };
        let f: Vec<&str> = e.split_whitespace().collect();
        let [mi, h, dom, mon, dow] = f[..] else {
            return Err(format!(
                "{expr:?}: a schedule has 5 fields (minute hour day month weekday) or is @hourly, @daily, ..."
            ));
        };
        let err = |what: &str, e: String| format!("{expr:?}: {what}: {e}");
        let (minute, _) = field(mi, 0, 59, &[], 0).map_err(|e| err("minute", e))?;
        let (hour, _) = field(h, 0, 23, &[], 0).map_err(|e| err("hour", e))?;
        let (d, dom_any) = field(dom, 1, 31, &[], 0).map_err(|e| err("day of month", e))?;
        let (m, _) = field(mon, 1, 12, &MONTHS, 1).map_err(|e| err("month", e))?;
        let (mut w, dow_any) = field(dow, 0, 7, &DAYS, 0).map_err(|e| err("day of week", e))?;
        if w & (1 << 7) != 0 {
            w |= 1; // 7 is Sunday too
        }
        Ok(Self {
            minute,
            hour: hour as u32,
            dom: d as u32,
            month: m as u16,
            dow: (w & 0x7f) as u8,
            dom_any,
            dow_any,
        })
    }

    pub fn matches(&self, t: DateTime) -> bool {
        let bit = |bits: u64, v: i8| bits & (1 << v as u32) != 0;
        if !bit(self.minute, t.minute())
            || !bit(self.hour as u64, t.hour())
            || !bit(self.month as u64, t.month())
        {
            return false;
        }
        let dom = bit(self.dom as u64, t.day());
        let dow = bit(self.dow as u64, t.weekday().to_sunday_zero_offset());
        match (self.dom_any, self.dow_any) {
            (false, false) => dom || dow,
            _ => dom && dow,
        }
    }

    /// The first matching minute after `t`, within a year.
    pub fn next_after(&self, t: &jiff::ZonedDateTime) -> Option<jiff::ZonedDateTime> {
        let mut z = t
            .round(
                jiff::ZonedDateTimeRound::new()
                    .smallest(jiff::Unit::Minute)
                    .mode(jiff::RoundMode::Floor),
            )
            .ok()?;
        let end = t.checked_add(jiff::Span::new().days(366)).ok()?;
        while z < end {
            z = z.checked_add(jiff::Span::new().minutes(1)).ok()?;
            let dt = z.datetime();
            let hour_ok = self.hour & (1 << dt.hour() as u32) != 0;
            if !hour_ok {
                // Skip to the next hour.
                z = z
                    .checked_add(jiff::Span::new().minutes(59 - dt.minute() as i64))
                    .ok()?;
                continue;
            }
            if self.matches(dt) {
                return Some(z);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn parse_and_match() {
        let c = Cron::parse("*/15 2-4 * * *").unwrap();
        assert!(c.matches(date(2026, 11, 20).at(3, 45, 0, 0)));
        assert!(!c.matches(date(2026, 11, 20).at(5, 0, 0, 0)));
        assert!(!c.matches(date(2026, 11, 20).at(3, 10, 0, 0)));
        // 2026-11-20 is a Friday.
        let fri = Cron::parse("0 12 * * fri").unwrap();
        assert!(fri.matches(date(2026, 11, 20).at(12, 0, 0, 0)));
        assert!(!fri.matches(date(2026, 11, 21).at(12, 0, 0, 0)));
        // Both day fields restricted: either matches.
        let either = Cron::parse("0 0 1 * mon").unwrap();
        assert!(either.matches(date(2026, 11, 23).at(0, 0, 0, 0))); // a Monday
        assert!(either.matches(date(2026, 12, 1).at(0, 0, 0, 0)));
        assert!(
            Cron::parse("0 0 * * 7")
                .unwrap()
                .matches(date(2026, 11, 22).at(0, 0, 0, 0))
        );
        assert_eq!(
            Cron::parse("@daily").unwrap(),
            Cron::parse("0 0 * * *").unwrap()
        );
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* * * * * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "x * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn next_run_in_a_time_zone() {
        let tz = jiff::tz::TimeZone::get("Asia/Seoul").unwrap();
        let now = date(2026, 11, 20)
            .at(14, 7, 30, 0)
            .to_zoned_datetime(tz)
            .unwrap();
        let c = Cron::parse("0 */6 * * *").unwrap();
        let next = c.next_after(&now).unwrap();
        assert_eq!(next.datetime(), date(2026, 11, 20).at(18, 0, 0, 0));
        let monthly = Cron::parse("@monthly").unwrap().next_after(&now).unwrap();
        assert_eq!(monthly.datetime(), date(2026, 12, 1).at(0, 0, 0, 0));
    }
}
