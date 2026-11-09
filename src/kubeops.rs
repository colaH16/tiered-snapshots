//! Thin Kubernetes API layer: PVC checks, VolumeSnapshot create/get/list/delete.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::Client;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, PostParams,
    Preconditions,
};
use serde_json::{Value, json};

use crate::config::{ClaimSource, Config};
use crate::plan::Snap;

pub struct Kube {
    pvcs: Api<PersistentVolumeClaim>,
    snaps: Api<DynamicObject>,
}

pub enum Created {
    New,
    AlreadyExists,
}

pub fn api_code(e: &kube::Error) -> Option<u16> {
    match e {
        kube::Error::Api(status) => Some(status.code),
        _ => None,
    }
}

impl Kube {
    pub async fn new(cfg: &Config) -> Result<Self> {
        let client = Client::try_default().await.context("kubernetes client")?;
        let gvk = GroupVersionKind::gvk("snapshot.storage.k8s.io", "v1", "VolumeSnapshot");
        let ar = ApiResource::from_gvk_with_plural(&gvk, "volumesnapshots");
        Ok(Kube {
            pvcs: Api::namespaced(client.clone(), &cfg.namespace),
            snaps: Api::namespaced_with(client, &cfg.namespace, &ar),
        })
    }

    /// Resolve and validate the claims to snapshot: each must be Bound (and use the
    /// required StorageClass, if configured).
    pub async fn claims(&self, cfg: &Config) -> Result<BTreeSet<String>> {
        let pvcs: Vec<PersistentVolumeClaim> = match &cfg.claims {
            ClaimSource::Names(names) => {
                let mut out = vec![];
                for name in names {
                    out.push(
                        self.pvcs
                            .get(name)
                            .await
                            .with_context(|| format!("get PVC {name}"))?,
                    );
                }
                out
            }
            ClaimSource::Selector(sel) => {
                let list = self.pvcs.list(&ListParams::default().labels(sel)).await?;
                if list.items.is_empty() {
                    bail!("CLAIM_SELECTOR {sel:?} matched no PVCs");
                }
                list.items
            }
        };
        let mut names = BTreeSet::new();
        for pvc in pvcs {
            let name = pvc.metadata.name.clone().unwrap_or_default();
            let phase = pvc.status.as_ref().and_then(|s| s.phase.as_deref());
            if phase != Some("Bound") {
                bail!("PVC {name} is not Bound (phase {phase:?})");
            }
            if let Some(want) = &cfg.require_storage_class {
                let got = pvc
                    .spec
                    .as_ref()
                    .and_then(|s| s.storage_class_name.as_deref());
                if got != Some(want.as_str()) {
                    bail!("PVC {name} uses StorageClass {got:?}, expected {want:?}");
                }
            }
            if name.len() + 14 > 253 {
                bail!("PVC name {name} too long for a snapshot name");
            }
            names.insert(name);
        }
        Ok(names)
    }

    pub async fn create(&self, cfg: &Config, claim: &str, name: &str) -> Result<Created> {
        let obj: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "snapshot.storage.k8s.io/v1",
            "kind": "VolumeSnapshot",
            "metadata": {
                "name": name,
                "namespace": cfg.namespace,
                "labels": {
                    cfg.policy_label.clone(): cfg.policy,
                    "app.kubernetes.io/managed-by": "tiered-snapshots",
                },
            },
            "spec": {
                "volumeSnapshotClassName": cfg.snapshot_class,
                "source": {"persistentVolumeClaimName": claim},
            },
        }))?;
        match self.snaps.create(&PostParams::default(), &obj).await {
            Ok(_) => Ok(Created::New),
            Err(e) if api_code(&e) == Some(409) => Ok(Created::AlreadyExists),
            Err(e) => Err(e).with_context(|| format!("create VolumeSnapshot {name}")),
        }
    }

    pub async fn get(&self, cfg: &Config, name: &str) -> Result<Snap> {
        let obj = self
            .snaps
            .get(name)
            .await
            .with_context(|| format!("get VolumeSnapshot {name}"))?;
        to_snap(&obj, Some(&cfg.policy_label))
    }

    pub async fn list(&self, cfg: &Config) -> Result<Vec<Snap>> {
        let selector = format!("{} in ({})", cfg.policy_label, cfg.policies().join(","));
        let mut out = vec![];
        let mut token: Option<String> = None;
        loop {
            let mut lp = ListParams::default().labels(&selector).limit(200);
            if let Some(t) = &token {
                lp = lp.continue_token(t);
            }
            let page = self.snaps.list(&lp).await.context("list VolumeSnapshots")?;
            for obj in &page.items {
                out.push(to_snap(obj, Some(&cfg.policy_label))?);
            }
            token = page.metadata.continue_.filter(|t| !t.is_empty());
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Delete with uid + resourceVersion preconditions. Returns false when the
    /// object is already gone or changed underneath us (left for the next run).
    pub async fn delete(&self, name: &str, uid: &str, resource_version: &str) -> Result<bool> {
        let dp = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.to_string()),
                resource_version: Some(resource_version.to_string()),
            }),
            ..DeleteParams::default()
        };
        match self.snaps.delete(name, &dp).await {
            Ok(_) => Ok(true),
            Err(e) if matches!(api_code(&e), Some(404) | Some(409)) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("delete VolumeSnapshot {name}")),
        }
    }
}

fn to_snap(obj: &DynamicObject, policy_label: Option<&str>) -> Result<Snap> {
    let v = serde_json::to_value(obj)?;
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(String::from);
    let created: Option<DateTime<Utc>> = s("/metadata/creationTimestamp")
        .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| t.with_timezone(&Utc));
    let policy = match policy_label {
        Some(key) => v
            .pointer("/metadata/labels")
            .and_then(|l| l.get(key))
            .and_then(Value::as_str)
            .map(String::from),
        None => None,
    };
    Ok(Snap {
        name: s("/metadata/name").unwrap_or_default(),
        namespace: s("/metadata/namespace").unwrap_or_default(),
        uid: s("/metadata/uid").unwrap_or_default(),
        resource_version: s("/metadata/resourceVersion").unwrap_or_default(),
        policy,
        class: s("/spec/volumeSnapshotClassName"),
        claim: s("/spec/source/persistentVolumeClaimName"),
        created,
        ready: v.pointer("/status/readyToUse").and_then(Value::as_bool) == Some(true),
        error: v.pointer("/status/error").is_some_and(|e| !e.is_null()),
        deleting: v
            .pointer("/metadata/deletionTimestamp")
            .is_some_and(|e| !e.is_null()),
    })
}
