//! Configuration from environment variables.
//!
//! Every knob is an env var so a CronJob manifest stays the single source of truth.

use std::collections::BTreeSet;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use chrono::Duration;

use crate::zone::{self, Zone};

/// One thinning level: snapshots older than `after` keep 1 of every `keep_every`
/// snapshots of the previous level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub after: Duration,
    pub keep_every: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimSource {
    Names(BTreeSet<String>),
    Selector(String),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub namespace: String,
    pub claims: ClaimSource,
    /// None = detect per PVC from its StorageClass driver (needs a read-only ClusterRole).
    pub snapshot_class: Option<String>,
    pub policy_label: String,
    pub policy: String,
    pub adopt_policies: Vec<String>,
    /// Level 1: base snapshot interval. Must match the CronJob schedule.
    pub interval: Duration,
    /// Level 2..N, sorted by `after`.
    pub tiers: Vec<Tier>,
    pub max_age: Duration,
    pub timezone: Zone,
    /// Where `timezone` came from, for the run log.
    pub timezone_source: String,
    pub ready_timeout: StdDuration,
    pub require_storage_class: Option<String>,
    pub dry_run: bool,
    pub status_file: Option<String>,
    pub status_lock_file: Option<String>,
    pub job_name: String,
    pub deferred_exit_code: i32,
}

impl Config {
    /// All label values this instance treats as its own.
    pub fn policies(&self) -> Vec<String> {
        let mut all = vec![self.policy.clone()];
        all.extend(self.adopt_policies.iter().cloned());
        all
    }

    /// Bucket width for each tier: interval × product of keep_every up to that tier.
    pub fn bucket_widths(&self) -> Vec<Duration> {
        let mut width = self.interval;
        self.tiers
            .iter()
            .map(|t| {
                width = width * t.keep_every as i32;
                width
            })
            .collect()
    }

    pub fn from_env() -> Result<Self> {
        let get = |k: &str| std::env::var(k).ok();
        let probe = zone::Probe::system(get("TIMEZONE").filter(|v| !v.trim().is_empty()));
        Self::from_lookup_with_zone(get, &probe)
    }

    #[cfg(test)]
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let probe = zone::Probe {
            timezone: get("TIMEZONE"),
            tz: None,
            localtime_link: None,
            localtime_exists: false,
        };
        Self::from_lookup_with_zone(get, &probe)
    }

    pub fn from_lookup_with_zone(
        get: impl Fn(&str) -> Option<String>,
        probe: &zone::Probe,
    ) -> Result<Self> {
        let req = |k: &str| -> Result<String> {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .with_context(|| format!("{k} is required"))
        };
        let opt = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let namespace = match opt("NAMESPACE") {
            Some(ns) => ns,
            None => {
                std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
                    .map(|s| s.trim().to_string())
                    .context("NAMESPACE is required outside a pod")?
            }
        };

        let claims = match (opt("CLAIMS"), opt("CLAIM_SELECTOR")) {
            (Some(_), Some(_)) => bail!("set only one of CLAIMS or CLAIM_SELECTOR"),
            (Some(list), None) => {
                let names: BTreeSet<String> = list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if names.is_empty() {
                    bail!("CLAIMS is empty");
                }
                ClaimSource::Names(names)
            }
            (None, Some(sel)) => ClaimSource::Selector(sel),
            (None, None) => bail!("CLAIMS or CLAIM_SELECTOR is required"),
        };

        let interval = parse_duration(&req("INTERVAL")?).context("INTERVAL")?;
        let tiers = match opt("TIERS") {
            Some(spec) => parse_tiers(&spec).context("TIERS")?,
            None => Vec::new(),
        };
        let max_age = parse_duration(&req("MAX_AGE")?).context("MAX_AGE")?;
        let (timezone, timezone_source) = zone::resolve(probe)?;
        let ready_timeout = parse_duration(&opt("READY_TIMEOUT").unwrap_or_else(|| "15m".into()))
            .context("READY_TIMEOUT")?
            .to_std()?;

        let policy = req("POLICY")?;
        let adopt_policies: Vec<String> = opt("ADOPT_POLICIES")
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != policy)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        for value in std::iter::once(&policy).chain(adopt_policies.iter()) {
            if !is_label_value(value) {
                bail!("invalid policy label value {value:?}");
            }
        }

        let dry_run = match opt("DRY_RUN").as_deref() {
            None | Some("false") | Some("0") => false,
            Some("true") | Some("1") => true,
            Some(other) => bail!("DRY_RUN must be true/false, got {other:?}"),
        };

        let cfg = Config {
            namespace,
            claims,
            snapshot_class: opt("SNAPSHOT_CLASS"),
            policy_label: opt("POLICY_LABEL").unwrap_or_else(|| "tiered-snapshots/policy".into()),
            policy,
            adopt_policies,
            interval,
            tiers,
            max_age,
            timezone,
            timezone_source,
            ready_timeout,
            require_storage_class: opt("REQUIRE_STORAGE_CLASS"),
            dry_run,
            status_file: opt("STATUS_FILE"),
            status_lock_file: opt("STATUS_LOCK_FILE"),
            job_name: opt("JOB_NAME").unwrap_or_default(),
            deferred_exit_code: opt("DEFERRED_EXIT_CODE")
                .map(|v| v.parse().context("DEFERRED_EXIT_CODE"))
                .transpose()?
                .unwrap_or(1),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Hard errors: settings that cannot describe a meaningful schedule.
    /// Every message says what is wrong, why, and how to fix it.
    pub fn validate(&self) -> Result<()> {
        if self.interval < Duration::minutes(1) {
            bail!(
                "INTERVAL={} is shorter than 1m: a CronJob cannot run more than once a minute.",
                fmt_dur(self.interval)
            );
        }
        let mut previous_start = Duration::zero();
        let mut previous_spacing = self.interval;
        for (i, tier) in self.tiers.iter().enumerate() {
            let level = i + 2;
            let spec = format!("{}:{}", fmt_dur(tier.after), tier.keep_every);
            if tier.keep_every < 2 {
                bail!(
                    "TIERS level {level} ({spec}): keep 1 of {} keeps every snapshot, so this \
                     level would change nothing. Use 2 or more, or remove the level.",
                    tier.keep_every
                );
            }
            if tier.after <= previous_start {
                bail!(
                    "TIERS level {level} ({spec}) starts at {}, which is not later than level {} \
                     ({}). Each level thins what the previous level kept, so levels must start \
                     at strictly increasing ages, e.g. TIERS=7d:3,30d:2.",
                    fmt_dur(tier.after),
                    level - 1,
                    if level == 2 {
                        "0d, i.e. the newest snapshot".to_string()
                    } else {
                        format!("starts at {}", fmt_dur(previous_start))
                    },
                );
            }
            if tier.after <= previous_spacing {
                bail!(
                    "TIERS level {level} ({spec}) starts at {}, but level {} only takes one \
                     snapshot every {}. Level {} would end before it holds a single snapshot; \
                     start level {level} later than {}.",
                    fmt_dur(tier.after),
                    level - 1,
                    fmt_dur(previous_spacing),
                    level - 1,
                    fmt_dur(previous_spacing),
                );
            }
            previous_start = tier.after;
            previous_spacing = previous_spacing * tier.keep_every as i32;
        }
        let last_level = self.tiers.len() + 1;
        if self.max_age <= previous_start {
            bail!(
                "MAX_AGE={} is not later than level {last_level}'s start ({}): every snapshot \
                 would be deleted before reaching level {last_level}. Raise MAX_AGE or drop \
                 the level.",
                fmt_dur(self.max_age),
                fmt_dur(previous_start)
            );
        }
        if self.max_age <= self.interval {
            bail!(
                "MAX_AGE={} is not longer than INTERVAL={}: each snapshot would be deleted \
                 before the next one is taken.",
                fmt_dur(self.max_age),
                fmt_dur(self.interval)
            );
        }
        Ok(())
    }

    /// Soft warnings: valid, but probably not what was intended.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = vec![];
        let day = Duration::days(1);
        if self.interval < day && day.num_seconds() % self.interval.num_seconds() != 0 {
            out.push(format!(
                "INTERVAL={} does not divide 24h: snapshot times shift from day to day instead \
                 of landing on the same clock times. Prefer 1h, 2h, 3h, 4h, 6h, 8h or 12h.",
                fmt_dur(self.interval)
            ));
        }
        for level in self.levels() {
            let span = level.end - level.start;
            if level.number > 1 && span < level.spacing * 2 {
                out.push(format!(
                    "level {} spans only {} but keeps one snapshot per {}: it will hold at most \
                     {} snapshot(s). Widen the gap to the next level / MAX_AGE, or drop it.",
                    level.number,
                    fmt_dur(span),
                    fmt_dur(level.spacing),
                    level.expected()
                ));
            }
        }
        let total: i64 = self.levels().iter().map(Level::expected).sum();
        if total > 500 {
            out.push(format!(
                "this schedule keeps about {total} snapshots per PVC; many CSI backends slow \
                 down with that many snapshots per volume. Consider a coarser level."
            ));
        }
        out
    }

    /// The effective levels, newest first: level 1 = every INTERVAL.
    pub fn levels(&self) -> Vec<Level> {
        let mut out = vec![];
        let mut start = Duration::zero();
        let mut spacing = self.interval;
        let mut keep_every = 1;
        for (i, tier) in self.tiers.iter().enumerate() {
            out.push(Level {
                number: i + 1,
                start,
                end: tier.after,
                spacing,
                keep_every,
            });
            start = tier.after;
            spacing = spacing * tier.keep_every as i32;
            keep_every = tier.keep_every;
        }
        out.push(Level {
            number: self.tiers.len() + 1,
            start,
            end: self.max_age,
            spacing,
            keep_every,
        });
        out
    }
}

/// One row of the retention table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Level {
    pub number: usize,
    /// Age range [start, end).
    pub start: Duration,
    pub end: Duration,
    /// One snapshot kept per `spacing`.
    pub spacing: Duration,
    /// 1 for level 1 ("keep all"), else "keep 1 of N" of the previous level.
    pub keep_every: u32,
}

impl Level {
    /// Steady-state snapshots per PVC in this level.
    pub fn expected(&self) -> i64 {
        (self.end - self.start).num_seconds() / self.spacing.num_seconds().max(1)
    }
}

/// `7d`, `12h`, `1d12h`, `90m`.
pub fn fmt_dur(d: Duration) -> String {
    let secs = d.num_seconds();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let mins = rem / 60;
    let mut s = String::new();
    if days > 0 {
        s += &format!("{days}d");
    }
    if hours > 0 {
        s += &format!("{hours}h");
    }
    if mins > 0 || s.is_empty() {
        s += &format!("{mins}m");
    }
    s
}

/// `90s`, `15m`, `4h`, `7d`, `2w`, or a combination like `1d12h`.
pub fn parse_duration(input: &str) -> Result<Duration> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    let mut total = Duration::zero();
    let mut digits = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if digits.is_empty() {
            bail!("invalid duration {input:?}");
        }
        let n: i64 = digits.parse()?;
        digits.clear();
        total += match ch {
            's' => Duration::seconds(n),
            'm' => Duration::minutes(n),
            'h' => Duration::hours(n),
            'd' => Duration::days(n),
            'w' => Duration::weeks(n),
            _ => bail!("unknown unit {ch:?} in {input:?} (use s, m, h, d, w)"),
        };
    }
    if !digits.is_empty() {
        bail!("missing unit in {input:?}");
    }
    if total <= Duration::zero() {
        bail!("duration must be positive: {input:?}");
    }
    Ok(total)
}

/// `7d:3,30d:2` → after 7 days keep 1 of 3; after 30 days keep 1 of 2 of those.
pub fn parse_tiers(spec: &str) -> Result<Vec<Tier>> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|part| {
            let (age, every) = part
                .split_once(':')
                .with_context(|| format!("tier {part:?} must look like <age>:<keep-every>"))?;
            Ok(Tier {
                after: parse_duration(age)?,
                keep_every: every
                    .trim()
                    .parse()
                    .with_context(|| format!("keep-every in {part:?}"))?,
            })
        })
        .collect()
}

fn is_label_value(v: &str) -> bool {
    let b = v.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.last().is_some_and(u8::is_ascii_alphanumeric)
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    const BASE: &[(&str, &str)] = &[
        ("NAMESPACE", "myapps"),
        ("CLAIMS", "a, b"),
        ("SNAPSHOT_CLASS", "rbd"),
        ("POLICY", "p-v2"),
        ("INTERVAL", "4h"),
        ("TIERS", "7d:3,30d:2"),
        ("MAX_AGE", "90d"),
        ("TIMEZONE", "Asia/Seoul"),
    ];

    #[test]
    fn durations() {
        assert_eq!(parse_duration("4h").unwrap(), Duration::hours(4));
        assert_eq!(parse_duration("1d12h").unwrap(), Duration::hours(36));
        assert_eq!(parse_duration("2w").unwrap(), Duration::days(14));
        for bad in ["", "4", "h", "4x", "0h", "-4h"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn full_config_and_bucket_widths() {
        let cfg = Config::from_lookup(env(BASE)).unwrap();
        assert_eq!(
            cfg.claims,
            ClaimSource::Names(["a".to_string(), "b".to_string()].into())
        );
        assert_eq!(
            cfg.bucket_widths(),
            vec![Duration::hours(12), Duration::hours(24)]
        );
        assert_eq!(cfg.policies(), vec!["p-v2".to_string()]);
        assert!(!cfg.dry_run);
        assert_eq!(cfg.deferred_exit_code, 1);
    }

    #[test]
    fn rejects_bad_tiers_and_max_age() {
        for (k, v) in [
            ("TIERS", "7d:1"),
            ("TIERS", "30d:2,7d:3"),
            ("TIERS", "7d"),
            ("MAX_AGE", "20d"),
            ("POLICY", "Bad Value"),
            ("DRY_RUN", "yes"),
        ] {
            let mut pairs: Vec<(&str, &str)> =
                BASE.iter().copied().filter(|(x, _)| *x != k).collect();
            pairs.push((k, v));
            assert!(Config::from_lookup(env(&pairs)).is_err(), "{k}={v}");
        }
        let mut both = BASE.to_vec();
        both.push(("CLAIM_SELECTOR", "app=x"));
        assert!(Config::from_lookup(env(&both)).is_err());
    }

    fn with(k: &'static str, v: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut pairs: Vec<(&str, &str)> = BASE.iter().copied().filter(|(x, _)| *x != k).collect();
        pairs.push((k, v));
        pairs
    }

    fn err(pairs: &[(&str, &str)]) -> String {
        format!("{:#}", Config::from_lookup(env(pairs)).unwrap_err())
    }

    #[test]
    fn errors_explain_why() {
        let e = err(&with("TIERS", "30d:3,7d:2"));
        assert!(
            e.contains("level 3") && e.contains("strictly increasing"),
            "{e}"
        );
        let e = err(&with("TIERS", "7d:3,7d:2"));
        assert!(e.contains("not later than level 2"), "{e}");
        let e = err(&with("TIERS", "7d:1"));
        assert!(e.contains("keeps every snapshot"), "{e}");
        let e = err(&with("TIERS", "3h:3"));
        assert!(e.contains("before it holds a single snapshot"), "{e}");
        let e = err(&with("MAX_AGE", "30d"));
        assert!(e.contains("level 3") && e.contains("Raise MAX_AGE"), "{e}");
        let e = err(&with("INTERVAL", "30s"));
        assert!(e.contains("once a minute"), "{e}");
    }

    #[test]
    fn levels_table_and_warnings() {
        let mut pairs = with("MAX_AGE", "60d");
        pairs.retain(|(k, _)| *k != "MAX_AGE");
        pairs.push(("MAX_AGE", "60d"));
        let cfg = Config::from_lookup(env(&pairs)).unwrap();
        let levels = cfg.levels();
        let rows: Vec<(String, String, String, u32, i64)> = levels
            .iter()
            .map(|l| {
                (
                    fmt_dur(l.start),
                    fmt_dur(l.end),
                    fmt_dur(l.spacing),
                    l.keep_every,
                    l.expected(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("0m".into(), "7d".into(), "4h".into(), 1, 42),
                ("7d".into(), "30d".into(), "12h".into(), 3, 46),
                ("30d".into(), "60d".into(), "1d".into(), 2, 30),
            ]
        );
        assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());

        let odd = Config::from_lookup(env(&with("INTERVAL", "5h"))).unwrap();
        assert!(
            odd.warnings()
                .iter()
                .any(|w| w.contains("does not divide 24h"))
        );
        let thin = Config::from_lookup(env(&with("MAX_AGE", "31d"))).unwrap();
        assert!(
            thin.warnings()
                .iter()
                .any(|w| w.contains("level 3 spans only 1d")),
            "{:?}",
            thin.warnings()
        );
    }

    #[test]
    fn fmt_durations() {
        assert_eq!(fmt_dur(Duration::hours(36)), "1d12h");
        assert_eq!(fmt_dur(Duration::days(7)), "7d");
        assert_eq!(fmt_dur(Duration::minutes(90)), "1h30m");
    }

    #[test]
    fn adopt_policies_dedupe_and_no_tiers_ok() {
        let mut pairs: Vec<(&str, &str)> = BASE
            .iter()
            .copied()
            .filter(|(k, _)| *k != "TIERS")
            .collect();
        pairs.push(("ADOPT_POLICIES", "old-v1, p-v2"));
        let cfg = Config::from_lookup(env(&pairs)).unwrap();
        assert_eq!(
            cfg.policies(),
            vec!["p-v2".to_string(), "old-v1".to_string()]
        );
        assert!(cfg.tiers.is_empty());
    }
}
