//! Configuration from environment variables.
//!
//! Every knob is an env var so a CronJob manifest stays the single source of truth.

use std::collections::BTreeSet;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use chrono::Duration;
use chrono_tz::Tz;

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
    pub snapshot_class: String,
    pub policy_label: String,
    pub policy: String,
    pub adopt_policies: Vec<String>,
    /// Level 1: base snapshot interval. Must match the CronJob schedule.
    pub interval: Duration,
    /// Level 2..N, sorted by `after`.
    pub tiers: Vec<Tier>,
    pub max_age: Duration,
    pub timezone: Tz,
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
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
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
        let timezone: Tz = opt("TIMEZONE")
            .unwrap_or_else(|| "UTC".into())
            .parse()
            .map_err(|e| anyhow::anyhow!("TIMEZONE: {e}"))?;
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
            snapshot_class: req("SNAPSHOT_CLASS")?,
            policy_label: opt("POLICY_LABEL").unwrap_or_else(|| "tiered-snapshots/policy".into()),
            policy,
            adopt_policies,
            interval,
            tiers,
            max_age,
            timezone,
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

    pub fn validate(&self) -> Result<()> {
        if self.interval < Duration::minutes(1) {
            bail!("INTERVAL must be at least 1m");
        }
        let mut previous = Duration::zero();
        for tier in &self.tiers {
            if tier.keep_every < 2 {
                bail!("keep-every must be >= 2 (got {})", tier.keep_every);
            }
            if tier.after <= previous {
                bail!("tier ages must be strictly increasing and > 0");
            }
            previous = tier.after;
        }
        if self.max_age <= previous || self.max_age <= self.interval {
            bail!("MAX_AGE must exceed the last tier age and INTERVAL");
        }
        Ok(())
    }
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
