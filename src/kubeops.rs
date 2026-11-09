//! Thin Kubernetes API layer: PVC checks, snapshot-class detection,
//! VolumeSnapshot create/get/list/delete.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::api::storage::v1::StorageClass;
use kube::Client;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, PostParams,
    Preconditions,
};
use serde_json::{Value, json};

use crate::config::{ClaimSource, Config};
use crate::plan::{Claims, Snap};

const DEFAULT_CLASS_ANNOTATION: &str = "snapshot.storage.kubernetes.io/is-default-class";

pub struct Kube {
    pvcs: Api<PersistentVolumeClaim>,
    snaps: Api<DynamicObject>,
    storage_classes: Api<StorageClass>,
    snapshot_classes: Api<DynamicObject>,
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
        let class_gvk =
            GroupVersionKind::gvk("snapshot.storage.k8s.io", "v1", "VolumeSnapshotClass");
        let class_ar = ApiResource::from_gvk_with_plural(&class_gvk, "volumesnapshotclasses");
        Ok(Kube {
            pvcs: Api::namespaced(client.clone(), &cfg.namespace),
            snaps: Api::namespaced_with(client.clone(), &cfg.namespace, &ar),
            storage_classes: Api::all(client.clone()),
            snapshot_classes: Api::all_with(client, &class_ar),
        })
    }

    /// Resolve and validate the claims to snapshot: each must be Bound (and use the
    /// required StorageClass, if configured). Returns PVC → VolumeSnapshotClass.
    pub async fn claims(&self, cfg: &Config) -> Result<Claims> {
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
        let mut claims = Claims::new();
        let mut classes: Option<Vec<ClassInfo>> = None;
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
            let class = match &cfg.snapshot_class {
                Some(class) => class.clone(),
                None => {
                    let sc = pvc
                        .spec
                        .as_ref()
                        .and_then(|s| s.storage_class_name.clone())
                        .with_context(|| {
                            format!("PVC {name} has no storageClassName; set SNAPSHOT_CLASS")
                        })?;
                    let driver = self
                        .storage_classes
                        .get(&sc)
                        .await
                        .map_err(rbac_hint)
                        .with_context(|| format!("get StorageClass {sc} (for PVC {name})"))?
                        .provisioner;
                    if classes.is_none() {
                        classes = Some(self.snapshot_class_infos().await?);
                    }
                    pick_class(&name, &sc, &driver, classes.as_deref().unwrap_or_default())?
                }
            };
            claims.insert(name, class);
        }
        Ok(claims)
    }

    async fn snapshot_class_infos(&self) -> Result<Vec<ClassInfo>> {
        let list = self
            .snapshot_classes
            .list(&ListParams::default())
            .await
            .map_err(rbac_hint)
            .context("list VolumeSnapshotClasses")?;
        Ok(list
            .items
            .iter()
            .map(|o| {
                let v = serde_json::to_value(o).unwrap_or(Value::Null);
                ClassInfo {
                    name: o.metadata.name.clone().unwrap_or_default(),
                    driver: v
                        .get("driver")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    default: o
                        .metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get(DEFAULT_CLASS_ANNOTATION))
                        .is_some_and(|v| v == "true"),
                }
            })
            .collect())
    }

    pub async fn create(
        &self,
        cfg: &Config,
        claim: &str,
        class: &str,
        name: &str,
    ) -> Result<Created> {
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
                "volumeSnapshotClassName": class,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    pub name: String,
    pub driver: String,
    pub default: bool,
}

/// Choose the VolumeSnapshotClass for a PVC whose StorageClass uses `driver`:
/// the only class for that driver, else the one marked default.
pub fn pick_class(pvc: &str, sc: &str, driver: &str, classes: &[ClassInfo]) -> Result<String> {
    let matching: Vec<&ClassInfo> = classes.iter().filter(|c| c.driver == driver).collect();
    let names = || {
        matching
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match matching.as_slice() {
        [] => bail!(
            "PVC {pvc}: StorageClass {sc} uses driver {driver}, but no VolumeSnapshotClass \
             has that driver. Create one, or set SNAPSHOT_CLASS."
        ),
        [only] => Ok(only.name.clone()),
        many => {
            let defaults: Vec<&&ClassInfo> = many.iter().filter(|c| c.default).collect();
            match defaults.as_slice() {
                [one] => Ok(one.name.clone()),
                [] => bail!(
                    "PVC {pvc}: driver {driver} has {} VolumeSnapshotClasses ({}) and none is \
                     annotated {DEFAULT_CLASS_ANNOTATION}=\"true\". Mark one as default, or set \
                     SNAPSHOT_CLASS.",
                    many.len(),
                    names()
                ),
                _ => bail!(
                    "PVC {pvc}: driver {driver} has several default VolumeSnapshotClasses ({}). \
                     Keep one default, or set SNAPSHOT_CLASS.",
                    names()
                ),
            }
        }
    }
}

/// Turn a 403 into an actionable message.
fn rbac_hint(e: kube::Error) -> anyhow::Error {
    if api_code(&e) == Some(403) {
        anyhow::anyhow!(
            "{e}. Detecting the snapshot class reads cluster-scoped StorageClasses and \
             VolumeSnapshotClasses: grant a ClusterRole with get on storageclasses and list on \
             volumesnapshotclasses (see deploy/example.yaml), or set SNAPSHOT_CLASS to stay \
             namespace-only."
        )
    } else {
        e.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, driver: &str, default: bool) -> ClassInfo {
        ClassInfo {
            name: name.into(),
            driver: driver.into(),
            default,
        }
    }

    #[test]
    fn class_selection() {
        let rbd = "rbd.csi.ceph.com";
        let one = [
            c("cephfs", "cephfs.csi.ceph.com", true),
            c("rbd-a", rbd, false),
        ];
        assert_eq!(pick_class("p", "sc", rbd, &one).unwrap(), "rbd-a");
        let two = [c("rbd-a", rbd, false), c("rbd-b", rbd, true)];
        assert_eq!(pick_class("p", "sc", rbd, &two).unwrap(), "rbd-b");
        let none_default = [c("rbd-a", rbd, false), c("rbd-b", rbd, false)];
        let e = pick_class("p", "sc", rbd, &none_default)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("rbd-a, rbd-b") && e.contains("SNAPSHOT_CLASS"),
            "{e}"
        );
        let both_default = [c("rbd-a", rbd, true), c("rbd-b", rbd, true)];
        assert!(
            pick_class("p", "sc", rbd, &both_default)
                .unwrap_err()
                .to_string()
                .contains("several default")
        );
        assert!(
            pick_class("p", "sc", "other", &one)
                .unwrap_err()
                .to_string()
                .contains("no VolumeSnapshotClass")
        );
    }
}
