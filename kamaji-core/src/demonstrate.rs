//! Pure, I/O-free scope parsing for `/demonstrate` -- sits apart from
//! `worker::demonstrate_job`'s orchestration the same way
//! `todo::parse_command`/`goal::parse_command` sit apart from their own
//! `worker/*_job.rs`, so `kamajid::transport::dispatch_routed_job` can
//! pre-validate args and skip the queue on a usage error before a job is
//! ever enqueued.

use chrono::{DateTime, Datelike, Utc};

pub const USAGE: &str = "Usage: /demonstrate [all|YYYY-Q1..4] (default: current quarter)";

/// Which facts a `/demonstrate` run considers. Facts have no open/closed
/// lifecycle (unlike todos/goals), so unlike `/align` this needs an
/// explicit bound -- `CurrentQuarter` is the default so a run's cost and
/// report size don't grow unboundedly as the bitacora accumulates over
/// years; `All`/`Quarter` are opt-in for a wider or historical scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    CurrentQuarter,
    Quarter { year: i32, quarter: u32 },
    All,
}

/// Parses `/demonstrate`'s optional single argument. No arg -> current
/// quarter. `"all"` -> full history. `"YYYY-Q[1-4]"` -> a specific quarter.
/// Anything else is a usage error.
pub fn parse_scope(args: &[String]) -> Result<Scope, String> {
    match args.first() {
        None => Ok(Scope::CurrentQuarter),
        Some(s) if s == "all" => Ok(Scope::All),
        Some(s) => parse_explicit_quarter(s).ok_or_else(|| USAGE.to_string()),
    }
}

fn parse_explicit_quarter(s: &str) -> Option<Scope> {
    let (year_part, quarter_part) = s.split_once("-Q")?;
    let year: i32 = year_part.parse().ok()?;
    let quarter: u32 = quarter_part.parse().ok()?;
    if !(1..=4).contains(&quarter) {
        return None;
    }
    Some(Scope::Quarter { year, quarter })
}

/// The three calendar months making up `quarter` (1-based: Q1 = Jan-Mar).
fn quarter_months(quarter: u32) -> [u32; 3] {
    let start = (quarter - 1) * 3 + 1;
    [start, start + 1, start + 2]
}

impl Scope {
    /// `None` means "scan every month ever written" (`All`); `Some(pairs)`
    /// gives the exact `(year, month)` pairs to scan, handed straight to
    /// `bitacora::list_facts`.
    pub fn months(&self, now: DateTime<Utc>) -> Option<Vec<(i32, u32)>> {
        match self {
            Scope::All => None,
            Scope::CurrentQuarter => {
                let quarter = (now.month() - 1) / 3 + 1;
                Some(
                    quarter_months(quarter)
                        .into_iter()
                        .map(|m| (now.year(), m))
                        .collect(),
                )
            }
            Scope::Quarter { year, quarter } => Some(
                quarter_months(*quarter)
                    .into_iter()
                    .map(|m| (*year, m))
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parse_scope_no_args_is_current_quarter() {
        assert_eq!(parse_scope(&[]).unwrap(), Scope::CurrentQuarter);
    }

    #[test]
    fn parse_scope_all() {
        let args: Vec<String> = vec!["all".to_string()];
        assert_eq!(parse_scope(&args).unwrap(), Scope::All);
    }

    #[test]
    fn parse_scope_explicit_quarter() {
        let args: Vec<String> = vec!["2026-Q2".to_string()];
        assert_eq!(
            parse_scope(&args).unwrap(),
            Scope::Quarter {
                year: 2026,
                quarter: 2
            }
        );
    }

    #[test]
    fn parse_scope_rejects_out_of_range_quarter() {
        let args: Vec<String> = vec!["2026-Q5".to_string()];
        assert!(parse_scope(&args).is_err());
        let args: Vec<String> = vec!["2026-Q0".to_string()];
        assert!(parse_scope(&args).is_err());
    }

    #[test]
    fn parse_scope_rejects_garbage() {
        let args: Vec<String> = vec!["bogus".to_string()];
        assert!(parse_scope(&args).is_err());
        let args: Vec<String> = vec!["2026".to_string()];
        assert!(parse_scope(&args).is_err());
    }

    #[test]
    fn scope_all_months_is_none() {
        let now = Utc.with_ymd_and_hms(2026, 7, 17, 0, 0, 0).unwrap();
        assert_eq!(Scope::All.months(now), None);
    }

    #[test]
    fn scope_current_quarter_derives_from_now() {
        let now = Utc.with_ymd_and_hms(2026, 7, 17, 0, 0, 0).unwrap();
        assert_eq!(
            Scope::CurrentQuarter.months(now),
            Some(vec![(2026, 7), (2026, 8), (2026, 9)])
        );
    }

    #[test]
    fn scope_explicit_quarter_months() {
        let now = Utc.with_ymd_and_hms(2026, 7, 17, 0, 0, 0).unwrap();
        let scope = Scope::Quarter {
            year: 2025,
            quarter: 1,
        };
        assert_eq!(
            scope.months(now),
            Some(vec![(2025, 1), (2025, 2), (2025, 3)])
        );
    }
}
