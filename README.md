# tiered-snapshots

Scheduled CSI `VolumeSnapshot`s with **tiered retention**, as a single Kubernetes
**CronJob** — no operator, no CRD, no resident pod, no cluster-wide RBAC.

```
level 1   every INTERVAL            ─ keep all        (e.g. every 4h for 7 days)
level 2   after 7d, keep 1 of 3     ─ 1 per 12h       (until 30 days)
level 3   after 30d, keep 1 of 2    ─ 1 per 24h       (until MAX_AGE)
          after MAX_AGE             ─ delete
```

```yaml
INTERVAL: 4h
TIERS: "7d:3,30d:2"     # <age>:<keep 1 of N>, any number of levels
MAX_AGE: 90d
```

Each run:

1. checks every target PVC is `Bound` (optionally on a required StorageClass);
2. creates one snapshot per PVC for the current `INTERVAL` window
   (deterministic name `<pvc>-<YYYYMMDD-HHMM>` in UTC, so a retried Job reuses it);
3. waits until **all** fresh snapshots are `readyToUse`;
4. **only then** deletes expired and thinned snapshots — and only ones it owns.

If a fresh snapshot errors or is not ready within `READY_TIMEOUT`, nothing is deleted.

## Why another snapshot tool?

| | tiered-snapshots | [Gemini](https://github.com/FairwindsOps/gemini) | [snapscheduler](https://github.com/backube/snapscheduler) |
|---|---|---|---|
| Runtime | CronJob (runs, exits) | operator + CRD | operator + CRD |
| RBAC | one namespaced Role (+ read-only ClusterRole only for class detection) | cluster-wide | cluster-wide |
| Retention model | age thresholds + "keep 1 of N" | count per granularity | count / age |
| Restore helpers | none (standard `dataSource`) | annotation-driven | none |
| Adopt pre-existing snapshots | yes (`ADOPT_POLICIES`) | no | no |

Pick Gemini if you want CRD-driven management and restore automation. Pick this if
you want the smallest moving part: a Job that runs every few hours and exits.

## How thinning works

For level *k* (k ≥ 2) the bucket width is `INTERVAL × N₂ × … × Nₖ`
(`4h → 12h → 24h` above), aligned to local midnight in `TIMEZONE`. Once a snapshot
is older than that level's age, only the **earliest ready** snapshot of its
(PVC, bucket) survives. Because buckets nest, a snapshot that survived level *k*
is never deleted when it reaches level *k+1* unless it is not that wider bucket's
earliest — survivors don't flip as time advances. A bucket with no ready snapshot
loses nothing, so a failed snapshot can never displace good ones.

Expected steady state per PVC for the example: 42 (7d × 6) + 46 (23d × 2) + 60 (60d × 1)
≈ **148 snapshots**.

## Ownership (what it will delete)

A snapshot is eligible only if **all** hold:

- in `NAMESPACE`, label `POLICY_LABEL` equals `POLICY` (or one of `ADOPT_POLICIES`);
- `spec.volumeSnapshotClassName == SNAPSHOT_CLASS`;
- source PVC is one of the current target PVCs;
- name is exactly `<pvc>-<8 digits>-<4 digits>`.

Snapshots you create by hand, other tools' snapshots, and snapshots of PVCs no
longer targeted are never touched. Deletes use `uid` + `resourceVersion`
preconditions.

## Configuration (environment variables)

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `CLAIMS` | one of | | comma-separated PVC names |
| `CLAIM_SELECTOR` | one of | | label selector for PVCs (needs `list` on PVCs) |
| `SNAPSHOT_CLASS` | no | detect | VolumeSnapshotClass; unset = detect per PVC (needs a ClusterRole, see below) |
| `POLICY` | yes | | label value identifying this schedule's snapshots |
| `INTERVAL` | yes | | level-1 period; **must match the CronJob schedule** |
| `TIERS` | no | none | `<age>:<N>,…`, ages strictly increasing, N ≥ 2 |
| `MAX_AGE` | yes | | delete anything older; must exceed the last tier age |
| `TIMEZONE` | no | `TZ` → `/etc/localtime` → UTC | IANA zone for window/bucket alignment; match `spec.timeZone` |
| `NAMESPACE` | no | pod namespace | |
| `POLICY_LABEL` | no | `tiered-snapshots/policy` | label key |
| `ADOPT_POLICIES` | no | | extra label values to treat as owned (migrations) |
| `REQUIRE_STORAGE_CLASS` | no | | refuse PVCs on any other StorageClass |
| `READY_TIMEOUT` | no | `15m` | wait for fresh snapshots before giving up |
| `DRY_RUN` | no | `false` | create/delete nothing; print the plan |
| `STATUS_FILE` | no | | write a JSON run record (see below) |
| `STATUS_LOCK_FILE` | no | | `flock` this file while writing `STATUS_FILE` |
| `JOB_NAME` | no | | recorded in the status file |
| `DEFERRED_EXIT_CODE` | no | `1` | exit code when fresh snapshots were not ready in time |

Durations: `30m`, `4h`, `7d`, `2w`, or combined like `1d12h`.

Invalid schedules are rejected before anything runs, with the reason and a fix —
for example `TIERS=30d:3,7d:2`:

```
config error: TIERS level 3 (7d:2) starts at 7d, which is not later than level 2 (starts at 30d).
Each level thins what the previous level kept, so levels must start at strictly increasing ages,
e.g. TIERS=7d:3,30d:2.
```

Legal but odd settings (an `INTERVAL` that does not divide 24h, a level too short to hold
a snapshot, hundreds of snapshots per PVC) run with a `warning:` line.

Exit codes: `0` success · `1` failure · `2` invalid configuration ·
`DEFERRED_EXIT_CODE` not ready in time.

### Time zone

Windows and buckets follow local wall-clock time. Resolution order: `TIMEZONE`
(IANA name) → `TZ` → `/etc/localtime` → UTC. Containers do not inherit the node's
zone; to follow the host, mount it read-only:

```yaml
volumeMounts: [{name: localtime, mountPath: /etc/localtime, readOnly: true}]
volumes: [{name: localtime, hostPath: {path: /etc/localtime, type: File}}]
```

Whatever you pick, keep the CronJob's `spec.timeZone` the same. The log states the
zone and where it came from.

### Snapshot class detection

With `SNAPSHOT_CLASS` unset, each PVC's StorageClass `provisioner` is matched against
VolumeSnapshotClass `driver`: a single match is used; with several, the one annotated
`snapshot.storage.kubernetes.io/is-default-class: "true"`; otherwise the run stops and
names the candidates. Both kinds are cluster-scoped, so this needs a read-only
ClusterRole (`get storageclasses`, `list volumesnapshotclasses`); setting
`SNAPSHOT_CLASS` keeps the tool namespace-only. Ownership is per PVC: only snapshots
in that PVC's class are ever pruned.

### Run log

```
retention policy "app-v1": INTERVAL=4h TIERS=7d:3,30d:2 MAX_AGE=60d TIMEZONE=Asia/Seoul
  time zone Asia/Seoul (from /etc/localtime → Asia/Seoul)
  pvc data → snapshot class csi-rbdplugin-snapclass (SNAPSHOT_CLASS)
  level  age           taken between                          spacing  rule          per PVC
  L1     0d → 7d       2026-11-09 14:53 … 2026-11-02 14:53         4h  keep all           42
  L2     7d → 30d      2026-11-02 14:53 … 2026-10-10 14:53        12h  keep 1 of 3        46
  L3     30d → 60d     2026-10-10 14:53 … 2026-09-10 14:53         1d  keep 1 of 2        30
                                                                              total      118
  older than 60d (2026-09-10 14:53) is deleted · 1 PVC(s) → about 118 snapshots in steady state
...
kept per PVC:
  pvc     L1    L2    L3  total  oldest kept
  data    42    46    30    118  2026-09-10 15:00 (59d23h ago)
  target  42    46    30    118  (steady-state target)
```

### Status file

```json
{"status":"success","at":1794195600.5,"job":"snap-29903220",
 "last_success":1794195600.5,"deferred_since":0}
```

Written atomically (temp file + rename + directory fsync, mode 0600);
`last_success` and `deferred_since` carry over between runs.

## Try it

```sh
# Read-only plan against your current kube context:
DRY_RUN=true NAMESPACE=my-app CLAIMS=data SNAPSHOT_CLASS=csi-rbdplugin-snapclass \
POLICY=my-app-v1 INTERVAL=4h TIERS=7d:3,30d:2 MAX_AGE=90d TIMEZONE=Asia/Seoul \
  cargo run
```

Deploy: see [`deploy/example.yaml`](deploy/example.yaml).

Restore: create a PVC with
`dataSource: {apiGroup: snapshot.storage.k8s.io, kind: VolumeSnapshot, name: <snapshot>}`
and validate it before switching workloads over.

## Migrating from another schedule

Keep your old label key, set `POLICY` to a new value and list the old value in
`ADOPT_POLICIES`. Old snapshots are then thinned by the same rules (names must
already follow `<pvc>-<YYYYMMDD-HHMM>`). Run with `DRY_RUN=true` first.

## Build

```sh
cargo test
docker build --target check .     # fmt + clippy + tests
docker build -t tiered-snapshots .
```

Images: `ghcr.io/colah16/tiered-snapshots` (`sha-<commit>`, `main`, release tags),
linux/amd64 and linux/arm64, on `distroless/static:nonroot`.

## Caveats

- Snapshots are crash-consistent and live in the same storage system as the
  volume. They are not off-cluster backups.
- `INTERVAL` / `TIMEZONE` must match the CronJob's `schedule` / `timeZone`;
  otherwise windows and buckets drift from when Jobs actually run.

## License

Apache-2.0
