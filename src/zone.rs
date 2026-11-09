//! Time zone resolution: `TIMEZONE` → `TZ` → `/etc/localtime` → UTC.
//!
//! Containers do not inherit the node's zone. To follow the host, mount the
//! node's `/etc/localtime` read-only (a plain TZif file works; chrono parses it).

use std::fmt;
use std::path::Path;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zone {
    /// An IANA zone from the embedded database.
    Named(Tz),
    /// The process-local zone (TZ rule string or a TZif `/etc/localtime`), via chrono.
    Local(String),
}

impl Zone {
    pub fn to_local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Zone::Named(tz) => at.with_timezone(tz).naive_local(),
            Zone::Local(_) => at.with_timezone(&Local).naive_local(),
        }
    }

    /// Earliest UTC instant for a local wall-clock time; None inside a DST gap.
    pub fn utc_of_local(&self, naive: &NaiveDateTime) -> Option<DateTime<Utc>> {
        match self {
            Zone::Named(tz) => tz
                .from_local_datetime(naive)
                .earliest()
                .map(|t| t.with_timezone(&Utc)),
            Zone::Local(_) => Local
                .from_local_datetime(naive)
                .earliest()
                .map(|t| t.with_timezone(&Utc)),
        }
    }

    pub fn format(&self, at: DateTime<Utc>, fmt: &str) -> String {
        self.to_local(at).format(fmt).to_string()
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Zone::Named(tz) => write!(f, "{tz}"),
            Zone::Local(name) => write!(f, "{name}"),
        }
    }
}

/// What the resolver could see on this machine.
pub struct Probe {
    pub timezone: Option<String>,
    pub tz: Option<String>,
    /// Symlink target of /etc/localtime, if it is a symlink.
    pub localtime_link: Option<String>,
    pub localtime_exists: bool,
}

impl Probe {
    pub fn system(timezone: Option<String>) -> Self {
        let path = Path::new("/etc/localtime");
        Probe {
            timezone,
            tz: std::env::var("TZ").ok().filter(|s| !s.trim().is_empty()),
            localtime_link: std::fs::read_link(path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
            localtime_exists: path.exists(),
        }
    }
}

/// Returns the zone and a short note on where it came from (for the run log).
pub fn resolve(p: &Probe) -> Result<(Zone, String)> {
    if let Some(name) = &p.timezone {
        let tz: Tz = name.parse().map_err(|_| {
            anyhow!(
                "TIMEZONE={name:?} is not an IANA zone name (e.g. Asia/Seoul, Europe/Berlin, UTC)"
            )
        })?;
        return Ok((Zone::Named(tz), "TIMEZONE".into()));
    }
    if let Some(tz) = &p.tz {
        let name = tz.trim_start_matches(':');
        return Ok(match name.parse::<Tz>() {
            Ok(named) => (Zone::Named(named), "TZ".into()),
            Err(_) => (Zone::Local(format!("TZ={tz}")), "TZ (rule string)".into()),
        });
    }
    if let Some(target) = &p.localtime_link
        && let Some((_, name)) = target.split_once("zoneinfo/")
        && let Ok(tz) = name.parse::<Tz>()
    {
        return Ok((Zone::Named(tz), format!("/etc/localtime → {name}")));
    }
    if p.localtime_exists {
        return Ok((
            Zone::Local("host /etc/localtime".into()),
            "/etc/localtime (TZif file)".into(),
        ));
    }
    Ok((
        Zone::Named(Tz::UTC),
        "default (no TIMEZONE, TZ or /etc/localtime)".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(timezone: Option<&str>, tz: Option<&str>, link: Option<&str>, exists: bool) -> Probe {
        Probe {
            timezone: timezone.map(String::from),
            tz: tz.map(String::from),
            localtime_link: link.map(String::from),
            localtime_exists: exists,
        }
    }

    #[test]
    fn precedence() {
        let seoul: Tz = "Asia/Seoul".parse().unwrap();
        let (z, src) = resolve(&probe(
            Some("Asia/Seoul"),
            Some("Europe/Berlin"),
            None,
            true,
        ))
        .unwrap();
        assert_eq!((z, src.as_str()), (Zone::Named(seoul), "TIMEZONE"));
        let (z, _) = resolve(&probe(
            None,
            Some(":Europe/Berlin"),
            Some("/usr/share/zoneinfo/Asia/Seoul"),
            true,
        ))
        .unwrap();
        assert_eq!(z, Zone::Named("Europe/Berlin".parse().unwrap()));
        let (z, src) = resolve(&probe(
            None,
            None,
            Some("../usr/share/zoneinfo/Asia/Seoul"),
            true,
        ))
        .unwrap();
        assert_eq!(z, Zone::Named(seoul));
        assert!(src.contains("Asia/Seoul"));
        let (z, _) = resolve(&probe(None, None, None, true)).unwrap();
        assert!(matches!(z, Zone::Local(_)));
        let (z, src) = resolve(&probe(None, None, None, false)).unwrap();
        assert_eq!(z, Zone::Named(Tz::UTC));
        assert!(src.starts_with("default"));
        let (z, _) = resolve(&probe(None, Some("KST-9"), None, false)).unwrap();
        assert!(matches!(z, Zone::Local(_)));
        assert!(resolve(&probe(Some("Mars/Base"), None, None, false)).is_err());
    }
}
