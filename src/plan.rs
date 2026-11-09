//! Pure retention planning: no I/O, fully unit-tested.
//!
//! Level 1 keeps every snapshot (one per `INTERVAL` window). Each further tier,
//! once a snapshot is older than `after`, keeps only the earliest *ready*
//! snapshot of every bucket whose width is `INTERVAL × keep_every × …` (cumulative),
//! aligned to local midnight of `TIMEZONE`. Anything older than `MAX_AGE` goes.
//! Buckets nest, so the survivor of a wide bucket is always also the survivor of
//! its narrower sub-bucket: a snapshot that survived tier N never gets deleted
//! later just because it reached tier N+1 — only its siblings do.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};

use crate::config::Config;
use crate::zone::Zone;

/// Target PVC → the VolumeSnapshotClass its snapshots use.
pub type Claims = BTreeMap<String, String>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snap {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub resource_version: String,
    pub policy: Option<String>,
    pub class: Option<String>,
    pub claim: Option<String>,
    pub created: Option<DateTime<Utc>>,
    pub ready: bool,
    pub error: bool,
    pub deleting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Expired,
    /// 1-based level number as the user sees it (level 2 = first tier).
    Thinned {
        level: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deletion {
    pub name: String,
    pub uid: String,
    pub resource_version: String,
    pub reason: Reason,
}

fn local_seconds(at: DateTime<Utc>, tz: &Zone) -> i64 {
    // Seconds since 1970-01-01T00:00 *local wall clock*, so day-multiple widths
    // align to local midnight.
    tz.to_local(at).and_utc().timestamp()
}

/// Start of the level-1 window containing `now`, as UTC.
pub fn window_start(now: DateTime<Utc>, interval: Duration, tz: &Zone) -> DateTime<Utc> {
    let width = interval.num_seconds().max(1);
    let secs = local_seconds(now, tz);
    let floored = secs - secs.rem_euclid(width);
    let naive: NaiveDateTime = DateTime::from_timestamp(floored, 0)
        .expect("in range")
        .naive_utc();
    match tz.utc_of_local(&naive) {
        Some(local) => local,
        // Window start fell into a DST gap: use the gap's end, never a future time.
        None => tz
            .utc_of_local(&(naive + Duration::hours(1)))
            .map(|l| l.min(now))
            .unwrap_or(now),
    }
}

/// Deterministic per-window name, so a retried Job addresses the same snapshot.
pub fn snapshot_name(claim: &str, window: DateTime<Utc>) -> String {
    format!("{claim}-{}", window.format("%Y%m%d-%H%M"))
}

fn has_stamp_suffix(name: &str, claim: &str) -> bool {
    let Some(rest) = name.strip_prefix(claim).and_then(|r| r.strip_prefix('-')) else {
        return false;
    };
    let b = rest.as_bytes();
    b.len() == 13
        && b[8] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 8 || c.is_ascii_digit())
}

/// Strict ownership: only snapshots this policy could have created are ever deleted.
pub fn owned(s: &Snap, cfg: &Config, claims: &Claims) -> bool {
    let Some(claim) = s.claim.as_deref() else {
        return false;
    };
    s.namespace == cfg.namespace
        && s.policy
            .as_ref()
            .is_some_and(|p| cfg.policies().contains(p))
        && claims
            .get(claim)
            .is_some_and(|class| s.class.as_deref() == Some(class.as_str()))
        && has_stamp_suffix(&s.name, claim)
}

pub fn plan(
    snaps: &[Snap],
    now: DateTime<Utc>,
    cfg: &Config,
    claims: &Claims,
    protected: &BTreeSet<String>,
) -> Vec<Deletion> {
    let candidates: Vec<(&Snap, DateTime<Utc>)> = snaps
        .iter()
        .filter(|s| !s.deleting && owned(s, cfg, claims))
        .filter_map(|s| s.created.map(|c| (s, c)))
        .collect();

    let mut out = Vec::new();
    let mut alive = Vec::new();
    for (s, created) in &candidates {
        if protected.contains(&s.name) {
            alive.push((*s, *created));
        } else if now - *created > cfg.max_age {
            out.push(deletion(s, Reason::Expired));
        } else {
            alive.push((*s, *created));
        }
    }

    let widths = cfg.bucket_widths();
    for (i, tier) in cfg.tiers.iter().enumerate() {
        let width = widths[i].num_seconds();
        let next_after = cfg.tiers.get(i + 1).map(|t| t.after);
        let bucket = |c: DateTime<Utc>| local_seconds(c, &cfg.timezone).div_euclid(width);
        // Keeper per (claim, bucket): earliest ready snapshot among ALL live ones,
        // regardless of their own age, so boundaries between tiers stay consistent.
        let mut keepers: HashMap<(&str, i64), (DateTime<Utc>, &str)> = HashMap::new();
        for (s, c) in &alive {
            if !s.ready || s.error {
                continue;
            }
            let key = (s.claim.as_deref().unwrap_or_default(), bucket(*c));
            let entry = keepers.entry(key).or_insert((*c, s.name.as_str()));
            if (*c, s.name.as_str()) < *entry {
                *entry = (*c, s.name.as_str());
            }
        }
        for (s, c) in &alive {
            let age = now - *c;
            let in_tier = age >= tier.after && next_after.is_none_or(|n| age < n);
            if !in_tier || protected.contains(&s.name) {
                continue;
            }
            let key = (s.claim.as_deref().unwrap_or_default(), bucket(*c));
            // No ready member in the bucket → delete nothing there.
            if let Some((_, keeper)) = keepers.get(&key)
                && *keeper != s.name
            {
                out.push(deletion(s, Reason::Thinned { level: i + 2 }));
            }
        }
    }
    out
}

fn deletion(s: &Snap, reason: Reason) -> Deletion {
    Deletion {
        name: s.name.clone(),
        uid: s.uid.clone(),
        resource_version: s.resource_version.clone(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClaimSource, Tier};
    use chrono::Timelike;

    fn cfg(tiers: Vec<Tier>, max_age: Duration) -> Config {
        Config {
            namespace: "myapps".into(),
            claims: ClaimSource::Names(["a".to_string(), "b".to_string()].into()),
            snapshot_class: Some("rbd".into()),
            policy_label: "x/policy".into(),
            policy: "p-v2".into(),
            adopt_policies: vec!["p-v1".into()],
            interval: Duration::hours(4),
            tiers,
            max_age,
            timezone: Zone::Named("Asia/Seoul".parse().unwrap()),
            timezone_source: String::new(),
            ready_timeout: std::time::Duration::from_secs(900),
            require_storage_class: None,
            dry_run: false,
            status_file: None,
            status_lock_file: None,
            job_name: String::new(),
            deferred_exit_code: 1,
        }
    }

    fn three_level() -> Config {
        cfg(
            vec![
                Tier {
                    after: Duration::days(7),
                    keep_every: 3,
                },
                Tier {
                    after: Duration::days(30),
                    keep_every: 2,
                },
            ],
            Duration::days(90),
        )
    }

    fn claims() -> Claims {
        [
            ("a".to_string(), "rbd".to_string()),
            ("b".to_string(), "rbd".to_string()),
        ]
        .into()
    }

    fn snap(claim: &str, created: DateTime<Utc>) -> Snap {
        Snap {
            name: snapshot_name(claim, created),
            namespace: "myapps".into(),
            uid: format!("uid-{claim}-{}", created.timestamp()),
            resource_version: "1".into(),
            policy: Some("p-v2".into()),
            class: Some("rbd".into()),
            claim: Some(claim.into()),
            created: Some(created),
            ready: true,
            error: false,
            deleting: false,
        }
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Every 4h on the KST grid (KST 00:00 = 15:00 UTC) from `start` to `now`.
    fn series(claim: &str, start: DateTime<Utc>, now: DateTime<Utc>, step_h: i64) -> Vec<Snap> {
        let mut out = vec![];
        let mut t = start;
        while t <= now {
            out.push(snap(claim, t));
            t += Duration::hours(step_h);
        }
        out
    }

    #[test]
    fn window_alignment_kst() {
        let tz = &Zone::Named("Asia/Seoul".parse().unwrap());
        // 2026-11-09 13:07 KST → 12:00 KST window = 03:00 UTC.
        let w = window_start(utc("2026-11-09T04:07:00Z"), Duration::hours(4), tz);
        assert_eq!(w, utc("2026-11-09T03:00:00Z"));
        assert_eq!(snapshot_name("a", w), "a-20261109-0300");
        // 01:30 KST → 00:00 KST = 15:00 UTC previous day.
        let w = window_start(utc("2026-11-08T16:30:00Z"), Duration::hours(4), tz);
        assert_eq!(w, utc("2026-11-08T15:00:00Z"));
    }

    #[test]
    fn window_alignment_dst_zone() {
        let tz = &Zone::Named("Europe/Berlin".parse().unwrap());
        // Spring forward 2026-03-29 02:00→03:00 local. 03:30 local (01:30 UTC),
        // 2h window → local 02:00 does not exist → clamp, never in the future.
        let now = utc("2026-03-29T01:30:00Z");
        let w = window_start(now, Duration::hours(2), tz);
        assert!(w <= now);
    }

    #[test]
    fn name_suffix_is_strict() {
        assert!(has_stamp_suffix("a-20261109-0300", "a"));
        assert!(!has_stamp_suffix("a-20261109-030", "a"));
        assert!(!has_stamp_suffix("ab-20261109-0300", "a"));
        assert!(!has_stamp_suffix("a-20261109x0300", "a"));
    }

    #[test]
    fn ownership_rules() {
        let c = three_level();
        let base = snap("a", utc("2026-11-01T03:00:00Z"));
        assert!(owned(&base, &c, &claims()));
        let mut legacy = base.clone();
        legacy.policy = Some("p-v1".into());
        assert!(owned(&legacy, &c, &claims()), "adopted policy");
        type Mutation = Box<dyn Fn(&mut Snap)>;
        let cases: Vec<Mutation> = vec![
            Box::new(|s| s.namespace = "other".into()),
            Box::new(|s| s.policy = Some("foreign".into())),
            Box::new(|s| s.policy = None),
            Box::new(|s| s.class = Some("other".into())),
            Box::new(|s| s.claim = Some("b".into())), // b exists, but name prefix is a-
            Box::new(|s| s.claim = Some("zzz".into())),
            Box::new(|s| s.name = "a-manual".into()),
        ];
        for mutate in cases {
            let mut s = base.clone();
            mutate(&mut s);
            assert!(!owned(&s, &c, &claims()), "{s:?}");
        }
    }

    #[test]
    fn three_levels_4h_12h_24h_then_max_age() {
        let c = three_level();
        let now = utc("2026-11-09T03:00:00Z");
        let all = series("a", utc("2026-07-01T15:00:00Z"), now, 4);
        let doomed: BTreeSet<String> = plan(&all, now, &c, &claims(), &BTreeSet::new())
            .into_iter()
            .map(|d| d.name)
            .collect();
        let kept: Vec<&Snap> = all.iter().filter(|s| !doomed.contains(&s.name)).collect();

        let age = |s: &Snap| now - s.created.unwrap();
        let level1: Vec<_> = kept.iter().filter(|s| age(s) < Duration::days(7)).collect();
        let level2: Vec<_> = kept
            .iter()
            .filter(|s| age(s) >= Duration::days(7) && age(s) < Duration::days(30))
            .collect();
        let level3: Vec<_> = kept
            .iter()
            .filter(|s| age(s) >= Duration::days(30))
            .collect();

        // Level 1: everything from the last 7 days survives (6/day).
        assert_eq!(
            level1.len(),
            all.iter().filter(|s| age(s) < Duration::days(7)).count()
        );
        // Level 2: 12h spacing, survivors on KST 00:00 / 12:00 (15:00 / 03:00 UTC).
        for s in &level2 {
            assert!(matches!(s.created.unwrap().hour(), 15 | 3), "{}", s.name);
        }
        assert!(
            (level2.len() as i64 - 23 * 2).abs() <= 1,
            "{}",
            level2.len()
        );
        // Level 3: 24h spacing, survivors on KST 00:00 only — except the bucket
        // straddling MAX_AGE, whose 00:00 member already expired.
        for s in level3.iter().filter(|s| age(s) <= Duration::days(89)) {
            assert_eq!(s.created.unwrap().hour(), 15, "{}", s.name);
        }
        assert!((level3.len() as i64 - 60).abs() <= 1, "{}", level3.len());
        // Nothing older than MAX_AGE.
        assert!(kept.iter().all(|s| age(s) <= Duration::days(90)));
    }

    #[test]
    fn steady_state_is_idempotent() {
        // Applying the plan twice deletes nothing the second time.
        let c = three_level();
        let now = utc("2026-11-09T03:00:00Z");
        let all = series("a", utc("2026-07-01T15:00:00Z"), now, 4);
        let first: BTreeSet<String> = plan(&all, now, &c, &claims(), &BTreeSet::new())
            .into_iter()
            .map(|d| d.name)
            .collect();
        let survivors: Vec<Snap> = all
            .into_iter()
            .filter(|s| !first.contains(&s.name))
            .collect();
        assert!(plan(&survivors, now, &c, &claims(), &BTreeSet::new()).is_empty());
    }

    #[test]
    fn survivors_never_flip_as_time_advances() {
        // Run the job every 4h for 100 days; a snapshot deleted is never one that an
        // earlier run had chosen to keep at a coarser level.
        let c = three_level();
        let start = utc("2026-07-01T15:00:00Z");
        let mut live: Vec<Snap> = vec![];
        let mut t = start;
        let mut ever_level3_kept: BTreeSet<String> = BTreeSet::new();
        while t <= start + Duration::days(100) {
            live.push(snap("a", t));
            let doomed: BTreeSet<String> = plan(&live, t, &c, &claims(), &BTreeSet::new())
                .into_iter()
                .map(|d| d.name)
                .collect();
            for d in &doomed {
                assert!(
                    !ever_level3_kept.contains(d) || {
                        let s = live.iter().find(|s| &s.name == d).unwrap();
                        t - s.created.unwrap() > c.max_age
                    },
                    "level-3 survivor {d} deleted before MAX_AGE"
                );
            }
            live.retain(|s| !doomed.contains(&s.name));
            for s in &live {
                if t - s.created.unwrap() >= Duration::days(30) {
                    ever_level3_kept.insert(s.name.clone());
                }
            }
            t += Duration::hours(4);
        }
        // 90 days retained, 7 dense days, 23 days at 12h, 60 days at 24h.
        assert!(
            (live.len() as i64 - (7 * 6 + 23 * 2 + 60)).abs() <= 3,
            "{}",
            live.len()
        );
    }

    #[test]
    fn legacy_6h_snapshots_are_thinned_too() {
        let c = three_level();
        let now = utc("2026-11-09T03:00:00Z");
        let mut all = series(
            "a",
            utc("2026-10-01T15:00:00Z"),
            utc("2026-10-25T15:00:00Z"),
            6,
        );
        for s in &mut all {
            s.policy = Some("p-v1".into());
        }
        let doomed = plan(&all, now, &c, &claims(), &BTreeSet::new());
        assert!(!doomed.is_empty());
        // KST 06:00 / 18:00 (21:00 / 09:00 UTC) share buckets with 00:00 / 12:00.
        for d in &doomed {
            let s = all.iter().find(|s| s.name == d.name).unwrap();
            assert!(
                matches!(s.created.unwrap().hour(), 21 | 9 | 3),
                "{}",
                s.name
            );
        }
    }

    #[test]
    fn unready_or_failed_never_displace_ready() {
        let c = three_level();
        let now = utc("2026-11-09T03:00:00Z");
        let base = utc("2026-10-20T15:00:00Z"); // KST 00:00, level 2
        let mut first = snap("a", base);
        first.ready = false;
        let second = snap("a", base + Duration::hours(4));
        let third = snap("a", base + Duration::hours(8));
        let doomed: BTreeSet<String> = plan(
            &[first.clone(), second.clone(), third.clone()],
            now,
            &c,
            &claims(),
            &BTreeSet::new(),
        )
        .into_iter()
        .map(|d| d.name)
        .collect();
        assert_eq!(doomed, [first.name.clone(), third.name.clone()].into());

        let mut lone1 = snap("b", base);
        let mut lone2 = snap("b", base + Duration::hours(4));
        lone1.ready = false;
        lone2.error = true;
        assert!(plan(&[lone1, lone2], now, &c, &claims(), &BTreeSet::new()).is_empty());
    }

    #[test]
    fn claims_independent_protected_and_deleting_skipped() {
        let c = three_level();
        let now = utc("2026-11-09T03:00:00Z");
        let base = utc("2026-10-20T15:00:00Z");
        let a: Vec<Snap> = [0, 4, 8]
            .iter()
            .map(|h| snap("a", base + Duration::hours(*h)))
            .collect();
        let b: Vec<Snap> = [4, 8]
            .iter()
            .map(|h| snap("b", base + Duration::hours(*h)))
            .collect();
        let mut all = a.clone();
        all.extend(b.clone());
        let protected: BTreeSet<String> = [a[2].name.clone()].into();
        let doomed: BTreeSet<String> = plan(&all, now, &c, &claims(), &protected)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(doomed, [a[1].name.clone(), b[1].name.clone()].into());

        let mut gone = snap("a", utc("2026-01-01T15:00:00Z"));
        gone.deleting = true;
        assert!(plan(&[gone], now, &c, &claims(), &BTreeSet::new()).is_empty());
    }

    #[test]
    fn expired_reason_and_unparseable_time_kept() {
        let c = three_level();
        let now = utc("2026-11-09T03:00:00Z");
        let old = snap("a", utc("2026-07-01T15:00:00Z"));
        let mut weird = snap("a", utc("2026-07-01T19:00:00Z"));
        weird.created = None;
        let d = plan(&[old.clone(), weird], now, &c, &claims(), &BTreeSet::new());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].reason, Reason::Expired);
        assert_eq!(d[0].name, old.name);
    }

    #[test]
    fn no_tiers_means_plain_max_age() {
        let c = cfg(vec![], Duration::days(30));
        let now = utc("2026-11-09T03:00:00Z");
        let all = series("a", utc("2026-09-01T15:00:00Z"), now, 4);
        let d = plan(&all, now, &c, &claims(), &BTreeSet::new());
        assert!(d.iter().all(|d| d.reason == Reason::Expired));
        let expired = all
            .iter()
            .filter(|s| now - s.created.unwrap() > Duration::days(30))
            .count();
        assert_eq!(d.len(), expired);
    }
}
