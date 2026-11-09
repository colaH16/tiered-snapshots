//! tiered-snapshots: CSI VolumeSnapshot scheduling with tiered (grandfather-father-son
//! style) retention, designed to run as a Kubernetes CronJob.
//!
//! One run = create this window's snapshots → wait until all are readyToUse →
//! only then prune expired / thinned snapshots it owns. A failed or slow fresh
//! snapshot never causes deletion.

mod config;
mod kubeops;
mod plan;
mod status;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::anyhow;
use chrono::Utc;

use config::Config;
use kubeops::{Created, Kube};
use plan::Reason;
use status::Outcome;

enum RunError {
    Deferred(String),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for RunError {
    fn from(e: anyhow::Error) -> Self {
        RunError::Failed(e)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("config error: {e:#}");
            return ExitCode::from(2);
        }
    };
    let result = run(&cfg).await;
    let (outcome, error, code) = match &result {
        Ok(()) => (Outcome::Success, None, 0),
        Err(RunError::Deferred(why)) => {
            eprintln!("deferred: {why}; no snapshots were deleted");
            (Outcome::Deferred, Some(why.clone()), cfg.deferred_exit_code)
        }
        Err(RunError::Failed(e)) => {
            eprintln!("failed: {e:#}");
            (Outcome::Failure, Some(format!("{e}")), 1)
        }
    };
    if let (Some(path), false) = (&cfg.status_file, cfg.dry_run) {
        let now = Utc::now().timestamp_millis() as f64 / 1000.0;
        if let Err(e) = status::write(
            Path::new(path),
            cfg.status_lock_file.as_deref().map(Path::new),
            outcome,
            &cfg.job_name,
            now,
            error.as_deref(),
        ) {
            eprintln!("status file write failed: {e:#}");
        }
    }
    ExitCode::from(code.clamp(0, 255) as u8)
}

async fn run(cfg: &Config) -> Result<(), RunError> {
    let kube = Kube::new(cfg).await?;
    let claims = kube.claims(cfg).await?;
    let now = Utc::now();
    let window = plan::window_start(now, cfg.interval, cfg.timezone);
    let fresh: Vec<(String, String)> = claims
        .iter()
        .map(|c| (c.clone(), plan::snapshot_name(c, window)))
        .collect();
    let protected: BTreeSet<String> = fresh.iter().map(|(_, n)| n.clone()).collect();
    println!(
        "window {} ({}), claims: {}",
        window.to_rfc3339(),
        cfg.timezone,
        claims.iter().cloned().collect::<Vec<_>>().join(", ")
    );

    if cfg.dry_run {
        for (_, name) in &fresh {
            println!("dry-run: would ensure {name}");
        }
    } else {
        for (claim, name) in &fresh {
            match kube.create(cfg, claim, name).await? {
                Created::New => println!("created {name}"),
                Created::AlreadyExists => println!("exists {name} (retry of this window)"),
            }
            let existing = kube.get(cfg, name).await?;
            if !plan::owned(&existing, cfg, &claims) || existing.claim.as_deref() != Some(claim) {
                return Err(anyhow!("{name} exists but is not ours; refusing to adopt").into());
            }
        }
        wait_ready(&kube, cfg, &claims, &fresh).await?;
    }

    let listed = kube.list(cfg).await?;
    let deletions = plan::plan(&listed, now, cfg, &claims, &protected);
    let (mut expired, mut thinned, mut skipped) = (0, 0, 0);
    for d in &deletions {
        let why = match d.reason {
            Reason::Expired => "max-age".to_string(),
            Reason::Thinned { level } => format!("thinned at level {level}"),
        };
        if cfg.dry_run {
            println!("dry-run: would delete {} ({why})", d.name);
        } else if kube.delete(&d.name, &d.uid, &d.resource_version).await? {
            println!("deleted {} ({why})", d.name);
        } else {
            skipped += 1;
            continue;
        }
        match d.reason {
            Reason::Expired => expired += 1,
            Reason::Thinned { .. } => thinned += 1,
        }
    }
    println!(
        "done: {} owned snapshots listed, {} expired, {} thinned, {} skipped (changed/gone){}",
        listed
            .iter()
            .filter(|s| plan::owned(s, cfg, &claims))
            .count(),
        expired,
        thinned,
        skipped,
        if cfg.dry_run { " [dry-run]" } else { "" }
    );
    Ok(())
}

async fn wait_ready(
    kube: &Kube,
    cfg: &Config,
    claims: &BTreeSet<String>,
    fresh: &[(String, String)],
) -> Result<(), RunError> {
    let deadline = Instant::now() + cfg.ready_timeout;
    loop {
        let mut all_ready = true;
        for (_, name) in fresh {
            let s = kube.get(cfg, name).await?;
            if !plan::owned(&s, cfg, claims) {
                return Err(anyhow!("{name} ownership changed").into());
            }
            if s.error {
                return Err(anyhow!("CSI reported an error on {name}").into());
            }
            all_ready &= s.ready;
        }
        if all_ready {
            println!("fresh snapshots ready: {}", fresh.len());
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(RunError::Deferred(format!(
                "fresh snapshots not ready after {:?}",
                cfg.ready_timeout
            )));
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
