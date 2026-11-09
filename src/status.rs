//! Optional run-status file for external monitors.
//!
//! JSON shape: `{"status": "success"|"deferred"|"failure", "at": <unix secs>,
//! "job": "<job name>", "last_success": <unix secs>, "deferred_since": <unix secs|0>,
//! "error": "<short reason>"?}`. Written atomically (temp file + rename + dir fsync),
//! optionally under an exclusive `flock` on a sibling lock file.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Deferred,
    Failure,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Deferred => "deferred",
            Outcome::Failure => "failure",
        }
    }
}

pub fn write(
    path: &Path,
    lock: Option<&Path>,
    outcome: Outcome,
    job: &str,
    now: f64,
    error: Option<&str>,
) -> Result<()> {
    let _guard = match lock {
        Some(lock) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc_o_nofollow())
                .open(lock)
                .with_context(|| format!("open lock {}", lock.display()))?;
            file.lock().context("flock")?;
            Some(file)
        }
        None => None,
    };
    let previous: Value = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let value = next_value(&previous, outcome, job, now, error);
    let dir = path.parent().context("status path has no parent")?;
    let tmp = dir.join(format!(
        ".pending-{}-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("status"),
        std::process::id()
    ));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(serde_json::to_string(&value)?.as_bytes())?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).context("rename status file");
    }
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn libc_o_nofollow() -> i32 {
    // O_NOFOLLOW on Linux (x86_64/aarch64); avoids a libc dependency.
    0o400000
}

pub fn next_value(
    previous: &Value,
    outcome: Outcome,
    job: &str,
    now: f64,
    error: Option<&str>,
) -> Value {
    let prev_f = |k: &str| previous.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let last_success = if outcome == Outcome::Success {
        now
    } else {
        prev_f("last_success")
    };
    let deferred_since = match outcome {
        Outcome::Deferred if prev_f("deferred_since") > 0.0 => prev_f("deferred_since"),
        Outcome::Deferred => now,
        _ => 0.0,
    };
    let mut value = json!({
        "status": outcome.as_str(),
        "at": now,
        "job": job,
        "last_success": last_success,
        "deferred_since": deferred_since,
    });
    if let Some(e) = error {
        value["error"] = json!(e);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_last_success_and_deferred_since() {
        let ok = next_value(&Value::Null, Outcome::Success, "j1", 100.0, None);
        assert_eq!(ok["last_success"], 100.0);
        assert_eq!(ok["deferred_since"], 0.0);
        let d1 = next_value(&ok, Outcome::Deferred, "j2", 200.0, Some("not ready"));
        assert_eq!(d1["last_success"], 100.0);
        assert_eq!(d1["deferred_since"], 200.0);
        assert_eq!(d1["error"], "not ready");
        let d2 = next_value(&d1, Outcome::Deferred, "j3", 300.0, None);
        assert_eq!(d2["deferred_since"], 200.0);
        let f = next_value(&d2, Outcome::Failure, "j4", 400.0, None);
        assert_eq!(f["deferred_since"], 0.0);
        assert_eq!(f["last_success"], 100.0);
        assert_eq!(f["job"], "j4");
    }

    #[test]
    fn atomic_write_with_lock() {
        let dir = std::env::temp_dir().join(format!("st-status-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run-snapshots.json");
        let lock = dir.join("outbox.lock");
        write(&path, Some(&lock), Outcome::Success, "job-a", 10.0, None).unwrap();
        write(
            &path,
            Some(&lock),
            Outcome::Failure,
            "job-b",
            20.0,
            Some("boom"),
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["status"], "failure");
        assert_eq!(v["last_success"], 10.0);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "no temp files left");
        fs::remove_dir_all(&dir).unwrap();
    }
}
