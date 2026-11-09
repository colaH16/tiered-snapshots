//! Human-readable run logs: the retention policy as a table, and what is kept.

use std::collections::BTreeMap;
use std::fmt::Write;

use chrono::{DateTime, Utc};

use crate::config::{Config, Level, fmt_dur};
use crate::plan::{Claims, Snap};

fn level_rule(l: &Level) -> String {
    if l.keep_every <= 1 {
        "keep all".into()
    } else {
        format!("keep 1 of {}", l.keep_every)
    }
}

/// ```text
/// level  age          taken between (Asia/Seoul)          spacing  rule         per PVC
/// L1     0d → 7d      2026-11-09 15:00 … 2026-11-02 15:00  4h       keep all          42
/// ```
pub fn policy_table(cfg: &Config, now: DateTime<Utc>, claims: &Claims) -> String {
    let tz = &cfg.timezone;
    let at = |d: chrono::Duration| tz.format(now - d, "%Y-%m-%d %H:%M");
    let levels = cfg.levels();
    let mut s = String::new();
    let _ = writeln!(
        s,
        "retention policy {:?}: INTERVAL={} TIERS={} MAX_AGE={} TIMEZONE={}",
        cfg.policy,
        fmt_dur(cfg.interval),
        if cfg.tiers.is_empty() {
            "-".to_string()
        } else {
            cfg.tiers
                .iter()
                .map(|t| format!("{}:{}", fmt_dur(t.after), t.keep_every))
                .collect::<Vec<_>>()
                .join(",")
        },
        fmt_dur(cfg.max_age),
        tz
    );
    let _ = writeln!(s, "  time zone {tz} (from {})", cfg.timezone_source);
    for (claim, class) in claims {
        let how = if cfg.snapshot_class.is_some() {
            "SNAPSHOT_CLASS"
        } else {
            "detected from StorageClass"
        };
        let _ = writeln!(s, "  pvc {claim} → snapshot class {class} ({how})");
    }
    let _ = writeln!(
        s,
        "  {:<5}  {:<12}  {:<37}  {:>7}  {:<12}  {:>7}",
        "level", "age", "taken between", "spacing", "rule", "per PVC"
    );
    for l in &levels {
        let _ = writeln!(
            s,
            "  L{:<4}  {:<12}  {:<37}  {:>7}  {:<12}  {:>7}",
            l.number,
            format!(
                "{} → {}",
                fmt_dur(l.start).replace("0m", "0d"),
                fmt_dur(l.end)
            ),
            format!("{} … {}", at(l.start), at(l.end)),
            fmt_dur(l.spacing),
            level_rule(l),
            l.expected()
        );
    }
    let total: i64 = levels.iter().map(Level::expected).sum();
    let _ = write!(
        s,
        "  {:>81}  {:>7}\n  older than {} ({}) is deleted · {} PVC(s) → about {} snapshots in steady state",
        "total",
        total,
        fmt_dur(cfg.max_age),
        at(cfg.max_age),
        claims.len(),
        total * claims.len() as i64
    );
    s
}

/// Count of kept snapshots per PVC per level, plus the oldest kept snapshot.
pub fn kept_table(
    cfg: &Config,
    now: DateTime<Utc>,
    claims: &Claims,
    kept: &[&Snap],
    dry_run: bool,
) -> String {
    let levels = cfg.levels();
    let level_of = |created: DateTime<Utc>| {
        let age = now - created;
        levels
            .iter()
            .position(|l| age < l.end)
            .unwrap_or(levels.len() - 1)
    };
    let mut counts: BTreeMap<&str, (Vec<usize>, Option<DateTime<Utc>>)> = claims
        .keys()
        .map(|c| (c.as_str(), (vec![0; levels.len()], None)))
        .collect();
    for s in kept {
        let (Some(claim), Some(created)) = (s.claim.as_deref(), s.created) else {
            continue;
        };
        if let Some((row, oldest)) = counts.get_mut(claim) {
            row[level_of(created)] += 1;
            if oldest.is_none_or(|o| created < o) {
                *oldest = Some(created);
            }
        }
    }
    let width = claims.keys().map(String::len).max().unwrap_or(3).max(3);
    let mut s = String::new();
    let _ = write!(
        s,
        "{} per PVC:\n  {:<width$}",
        if dry_run { "would keep" } else { "kept" },
        "pvc"
    );
    for l in &levels {
        let _ = write!(s, "  {:>4}", format!("L{}", l.number));
    }
    let _ = writeln!(s, "  {:>5}  oldest kept", "total");
    let mut target = String::new();
    for l in &levels {
        let _ = write!(target, "  {:>4}", l.expected());
    }
    for (claim, (row, oldest)) in &counts {
        let _ = write!(s, "  {claim:<width$}");
        for n in row {
            let _ = write!(s, "  {n:>4}");
        }
        let oldest = oldest
            .map(|o| {
                format!(
                    "{} ({} ago)",
                    cfg.timezone.format(o, "%Y-%m-%d %H:%M"),
                    fmt_dur(if now - o >= chrono::Duration::days(1) {
                        chrono::Duration::hours((now - o).num_hours())
                    } else {
                        now - o
                    })
                )
            })
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(s, "  {:>5}  {oldest}", row.iter().sum::<usize>());
    }
    let total: i64 = levels.iter().map(Level::expected).sum();
    let _ = write!(
        s,
        "  {:<width$}{target}  {total:>5}  (steady-state target)",
        "target"
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClaimSource, Tier};
    use crate::plan::snapshot_name;
    use chrono::Duration;

    fn cfg() -> Config {
        Config {
            namespace: "ns".into(),
            claims: ClaimSource::Names(["data".to_string()].into()),
            snapshot_class: Some("c".into()),
            policy_label: "l".into(),
            policy: "p".into(),
            adopt_policies: vec![],
            interval: Duration::hours(4),
            tiers: vec![
                Tier {
                    after: Duration::days(7),
                    keep_every: 3,
                },
                Tier {
                    after: Duration::days(30),
                    keep_every: 2,
                },
            ],
            max_age: Duration::days(60),
            timezone: crate::zone::Zone::Named("Asia/Seoul".parse().unwrap()),
            timezone_source: "TIMEZONE".into(),
            ready_timeout: std::time::Duration::from_secs(1),
            require_storage_class: None,
            dry_run: false,
            status_file: None,
            status_lock_file: None,
            job_name: String::new(),
            deferred_exit_code: 1,
        }
    }

    #[test]
    fn tables_render() {
        let c = cfg();
        let now = DateTime::parse_from_rfc3339("2026-11-09T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let three: Claims = ["a", "b", "data"]
            .iter()
            .map(|n| (n.to_string(), "c".to_string()))
            .collect();
        let p = policy_table(&c, now, &three);
        assert!(
            p.contains("pvc data → snapshot class c (SNAPSHOT_CLASS)"),
            "{p}"
        );
        assert!(p.contains("L1     0d → 7d"), "{p}");
        assert!(p.contains("2026-11-09 15:00 … 2026-11-02 15:00"), "{p}");
        assert!(p.contains("keep 1 of 3"), "{p}");
        assert!(p.contains("118"), "{p}");
        assert!(p.contains("about 354 snapshots"), "{p}");

        let snaps: Vec<Snap> = [1, 10, 40]
            .iter()
            .map(|d| {
                let t = now - Duration::days(*d);
                Snap {
                    name: snapshot_name("data", t),
                    claim: Some("data".into()),
                    created: Some(t),
                    ..Snap::default()
                }
            })
            .collect();
        let refs: Vec<&Snap> = snaps.iter().collect();
        let k = kept_table(
            &c,
            now,
            &[("data".to_string(), "c".to_string())].into(),
            &refs,
            false,
        );
        assert!(
            k.contains("data     1     1     1      3  2026-09-30 15:00 (40d ago)"),
            "{k}"
        );
        assert!(k.contains("42    46    30    118"), "{k}");
    }
}
