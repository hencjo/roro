//! Stop-the-publication-gate mark/sweep: serving never stops, existing streams stay open.
//! Do not make the sweep concurrent with publication without a durable write barrier.
use crate::{
    error::{AppError, Result},
    store::{Manifest, Repo, Store, blob_path, check_digest, upload_path},
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
};
pub const DAY: u64 = 86400;
pub const GRACE: u64 = 7 * DAY;
#[derive(Default, Debug, Serialize)]
pub struct Report {
    pub expired_tags: u64,
    pub removed_objects: u64,
    pub reclaimed_bytes: u64,
    pub removed_uploads: u64,
}

fn validate_manifest(store: &Store, digest: &str, manifest: &Manifest) -> Result<()> {
    use crate::store::{DOCKER_INDEX, DOCKER_MANIFEST, MAX_MANIFEST, OCI_INDEX, OCI_MANIFEST};
    if manifest.size > MAX_MANIFEST as u64 {
        return Err(AppError::unavailable(
            "oversized stored manifest; GC stopped",
        ));
    }
    let mut bytes = Vec::new();
    store
        .disk
        .open_read(&blob_path(digest))?
        .take(MAX_MANIFEST as u64 + 1)
        .read_to_end(&mut bytes)?;
    if crate::store::digest(&bytes) != digest || bytes.len() as u64 != manifest.size {
        return Err(AppError::unavailable("corrupt stored manifest; GC stopped"));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("schemaVersion").and_then(|v| v.as_u64()) != Some(2) {
        return Err(AppError::unavailable("invalid manifest schema; GC stopped"));
    }
    let descriptors: Vec<&serde_json::Value> = match manifest.media_type.as_str() {
        OCI_INDEX | DOCKER_INDEX => value
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| AppError::unavailable("invalid stored index"))?
            .iter()
            .collect(),
        OCI_MANIFEST | DOCKER_MANIFEST => {
            let mut descriptors = vec![
                value
                    .get("config")
                    .ok_or_else(|| AppError::unavailable("invalid stored config"))?,
            ];
            descriptors.extend(
                value
                    .get("layers")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| AppError::unavailable("invalid stored layers"))?,
            );
            descriptors
        }
        _ => return Err(AppError::unavailable("invalid stored media type")),
    };
    let mut dependencies = Vec::new();
    for descriptor in descriptors {
        let d = descriptor
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::unavailable("invalid stored descriptor"))?;
        check_digest(d).map_err(|_| AppError::unavailable("invalid stored dependency digest"))?;
        let size = descriptor
            .get("size")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| AppError::unavailable("invalid stored descriptor size"))?;
        if store.disk.open_read(&blob_path(d))?.metadata()?.len() != size {
            return Err(AppError::unavailable(
                "stored dependency size mismatch; GC stopped",
            ));
        }
        dependencies.push(d.to_owned());
    }
    if dependencies != manifest.dependencies {
        return Err(AppError::unavailable(
            "manifest dependency metadata mismatch; GC stopped",
        ));
    }
    Ok(())
}
fn mark(
    roots: impl IntoIterator<Item = String>,
    graph: &BTreeMap<String, Manifest>,
) -> BTreeSet<String> {
    let mut marked = BTreeSet::new();
    let mut pending: Vec<_> = roots.into_iter().collect();
    while let Some(d) = pending.pop() {
        if marked.insert(d.clone())
            && let Some(m) = graph.get(&d)
        {
            pending.extend(m.dependencies.iter().cloned());
        }
    }
    marked
}
pub fn run(store: &Store) -> Result<Report> {
    run_at(store, crate::now())
}
pub fn run_at(store: &Store, now: u64) -> Result<Report> {
    let Some(_leader) = store.disk.try_lock("gc-coordinator", true)? else {
        return Ok(Report::default());
    };
    let _gate = store.disk.lock("publication", true)?;
    let mut report = Report::default();
    let mut repos = Vec::<Repo>::new();
    let mut graph = BTreeMap::<String, Manifest>::new();
    // Load and validate the complete graph before expiring tags or deleting any object.
    for entry in fs::read_dir(store.disk.path(Path::new("repos"))?)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| AppError::unavailable("invalid repository filename"))?;
        if name.starts_with(".tmp-") {
            continue;
        }
        if !entry.file_type()?.is_file() || !name.ends_with(".json") {
            return Err(AppError::unavailable(
                "unexpected repository entry; GC stopped",
            ));
        }
        let relative = std::path::PathBuf::from("repos").join(name);
        let raw: Repo = serde_json::from_slice(&store.disk.read(&relative)?)?;
        if format!("{}.json", crate::store::repo_key(&raw.name)) != name {
            return Err(AppError::unavailable("repository filename mismatch"));
        }
        let repo = store
            .load_repo(&raw.name)?
            .ok_or_else(|| AppError::unavailable("repository disappeared"))?;
        for d in repo.blobs.iter().chain(repo.manifests.keys()) {
            if !store.disk.open_read(&blob_path(d))?.metadata()?.is_file() {
                return Err(AppError::unavailable("non-file CAS object"));
            }
        }
        for (d, m) in &repo.manifests {
            if store.disk.open_read(&blob_path(d))?.metadata()?.len() != m.size {
                return Err(AppError::unavailable("manifest size mismatch; GC stopped"));
            }
            if let Some(existing) = graph.get(d)
                && (existing.dependencies != m.dependencies
                    || existing.media_type != m.media_type
                    || existing.size != m.size)
            {
                return Err(AppError::unavailable(
                    "inconsistent manifest metadata; GC stopped",
                ));
            }
            if !graph.contains_key(d) {
                validate_manifest(store, d, m)?;
            }
            graph.insert(d.clone(), m.clone());
        }
        repos.push(repo);
    }
    let state_path = Path::new("gc/unreachable.json");
    let mut unreachable: BTreeMap<String, u64> = match store.disk.open_read(state_path) {
        Ok(f) => serde_json::from_reader(f)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e.into()),
    };
    for d in unreachable.keys() {
        check_digest(d).map_err(|_| AppError::unavailable("invalid GC checkpoint"))?;
    }
    let mut objects = BTreeMap::new();
    for entry in fs::read_dir(store.disk.path(Path::new("blobs"))?)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::unavailable("invalid CAS filename"))?;
        let d = format!("sha256:{name}");
        check_digest(&d).map_err(|_| AppError::unavailable("invalid CAS entry; GC stopped"))?;
        let meta = store.disk.open_read(&blob_path(&d))?.metadata()?;
        if !meta.is_file() {
            return Err(AppError::unavailable("non-file CAS entry"));
        }
        let modified = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(AppError::internal)?
            .as_secs();
        objects.insert(d, (meta.len(), modified));
    }
    for repo in &mut repos {
        let before = repo.tags.len();
        repo.tags
            .retain(|name, tag| name == "latest" || now.saturating_sub(tag.updated_at) < 30 * DAY);
        report.expired_tags += (before - repo.tags.len()) as u64;
    }
    let durable = mark(
        repos
            .iter()
            .flat_map(|r| r.tags.values().map(|t| t.digest.clone())),
        &graph,
    );
    for (d, (_, modified)) in &objects {
        if durable.contains(d) {
            unreachable.remove(d);
        } else {
            let since = unreachable.entry(d.clone()).or_insert(now);
            *since = (*since).max(*modified);
        }
    }
    // Untagged manifests are temporary roots during their grace period, keeping their layers alive.
    let grace_roots = graph
        .keys()
        .filter(|d| {
            unreachable
                .get(*d)
                .is_some_and(|since| now.saturating_sub(*since) < GRACE)
        })
        .cloned();
    let mut live = mark(grace_roots, &graph);
    live.extend(durable);
    let dead: BTreeSet<_> = objects
        .keys()
        .filter(|d| {
            !live.contains(*d)
                && unreachable
                    .get(*d)
                    .is_some_and(|since| now.saturating_sub(*since) >= GRACE)
        })
        .cloned()
        .collect();
    // First atomically remove all links. A crash may leak objects, never leave live metadata pointing at a deleted file.
    for repo in &mut repos {
        repo.blobs.retain(|d| !dead.contains(d));
        repo.manifests.retain(|d, _| !dead.contains(d));
        store.save_repo(repo)?;
    }
    store
        .disk
        .atomic(state_path, &serde_json::to_vec(&unreachable)?)?;
    for d in dead {
        store.disk.remove(&blob_path(&d))?;
        report.removed_objects += 1;
        report.reclaimed_bytes += objects[&d].0;
        unreachable.remove(&d);
    }
    unreachable.retain(|d, _| objects.contains_key(d));
    store
        .disk
        .atomic(state_path, &serde_json::to_vec(&unreachable)?)?;
    // Skip any upload with an active owner. There is no TTL-based lock stealing.
    for entry in fs::read_dir(store.disk.path(Path::new("uploads"))?)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::unavailable("invalid upload filename"))?;
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        upload_path(id, "json")?;
        let Some(_active) = store.disk.try_lock(&format!("upload-active:{id}"), true)? else {
            continue;
        };
        let Some(_upload) = store.disk.try_lock(&format!("upload:{id}"), true)? else {
            continue;
        };
        let u: crate::store::Upload =
            serde_json::from_slice(&store.disk.read(&upload_path(id, "json")?)?)?;
        if u.id != id {
            return Err(AppError::unavailable("upload checkpoint mismatch"));
        }
        if now.saturating_sub(u.updated_at) >= DAY {
            store.disk.remove(&upload_path(id, "json")?)?;
            store.disk.remove(&upload_path(id, "data")?)?;
            report.removed_uploads += 1;
        }
    }
    // Orphaned data and staging files can be left behind by SIGKILL or interrupted initialization.
    for dir in ["incoming", "uploads", "repos", "gc"] {
        for entry in fs::read_dir(store.disk.path(Path::new(dir))?)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| AppError::unavailable("invalid temporary filename"))?;
            if !entry.file_type()?.is_file() {
                return Err(AppError::unavailable("unexpected temporary entry"));
            }
            let modified = entry
                .metadata()?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(AppError::internal)?
                .as_secs();
            if now.saturating_sub(modified) < DAY {
                continue;
            }
            if dir == "incoming" && !name.starts_with(".tmp-") {
                if uuid::Uuid::parse_str(&name).is_err() {
                    continue;
                }
                let Some(_lease) = store.disk.try_lock(&format!("incoming:{name}"), true)? else {
                    continue;
                };
                store.disk.remove(&Path::new(dir).join(&name))?;
            } else if dir == "uploads" && name.ends_with(".data") {
                let id = name.trim_end_matches(".data");
                upload_path(id, "data")?;
                let Some(_active) = store.disk.try_lock(&format!("upload-active:{id}"), true)?
                else {
                    continue;
                };
                let Some(_lease) = store.disk.try_lock(&format!("upload:{id}"), true)? else {
                    continue;
                };
                if !store.disk.path(&upload_path(id, "json")?)?.exists() {
                    store.disk.remove(&upload_path(id, "data")?)?;
                }
            } else if name.starts_with(".tmp-") {
                store.disk.remove(&Path::new(dir).join(&name))?;
            }
        }
    }
    Ok(report)
}
