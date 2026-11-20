use crate::{
    error::{AppError, Result},
    fs::Disk,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub const MAX_MANIFEST: usize = 10 * 1024 * 1024;
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_INDEX: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub project_id: u64,
    pub project_path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tag {
    pub digest: String,
    pub updated_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub media_type: String,
    pub size: u64,
    pub dependencies: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    pub name: String,
    pub owner: Owner,
    pub blobs: BTreeSet<String>,
    pub manifests: BTreeMap<String, Manifest>,
    pub tags: BTreeMap<String, Tag>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    pub id: String,
    pub repo: String,
    pub owner: Owner,
    pub offset: u64,
    pub updated_at: u64,
}
#[derive(Clone)]
pub struct Store {
    pub disk: Disk,
}
pub struct Content {
    pub file: File,
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    name.split('/').all(|part| {
        let b = part.as_bytes();
        if b.is_empty() || !b[0].is_ascii_lowercase() && !b[0].is_ascii_digit() {
            return false;
        }
        let mut i = 0;
        while i < b.len() {
            if b[i].is_ascii_lowercase() || b[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            match b[i] {
                b'.' => i += 1,
                b'_' => {
                    i += 1;
                    if i < b.len() && b[i] == b'_' {
                        i += 1;
                    }
                }
                b'-' => {
                    while i < b.len() && b[i] == b'-' {
                        i += 1;
                    }
                }
                _ => return false,
            }
            if i == b.len() || !b[i].is_ascii_lowercase() && !b[i].is_ascii_digit() {
                return false;
            }
        }
        true
    })
}
pub fn check_name(name: &str) -> Result<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(AppError::bad("NAME_INVALID", "invalid repository name"))
    }
}
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && tag.bytes().enumerate().all(|(i, c)| {
            c.is_ascii_alphanumeric() || c == b'_' || i > 0 && (c == b'.' || c == b'-')
        })
}
pub fn check_digest(digest: &str) -> Result<()> {
    if digest.len() == 71
        && digest.starts_with("sha256:")
        && digest[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(AppError::bad(
            "DIGEST_INVALID",
            "only canonical sha256 digests are supported",
        ))
    }
}
pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}
pub fn repo_key(name: &str) -> String {
    hex::encode(Sha256::digest(name.as_bytes()))
}
pub fn blob_path(digest: &str) -> PathBuf {
    PathBuf::from(format!("blobs/{}", &digest[7..]))
}
pub fn upload_path(id: &str, suffix: &str) -> Result<PathBuf> {
    if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Err(AppError::missing("BLOB_UPLOAD_UNKNOWN"));
    }
    Ok(PathBuf::from(format!("uploads/{id}.{suffix}")))
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let disk = Disk::open(root)?;
        let _format = disk.lock("storage-format", true)?;
        let path = Path::new("gc/storage-format.json");
        match disk.open_read(path) {
            Ok(f) => {
                let format: serde_json::Value = serde_json::from_reader(f)?;
                if format != serde_json::json!({"version":1}) {
                    return Err(AppError::unavailable(
                        "unsupported storage format; do not mix registry versions",
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                disk.atomic(path, b"{\"version\":1}")?
            }
            Err(e) => return Err(e.into()),
        }
        Ok(Self { disk })
    }
    pub fn load_repo(&self, name: &str) -> Result<Option<Repo>> {
        check_name(name)?;
        let p = PathBuf::from(format!("repos/{}.json", repo_key(name)));
        match self.disk.open_read(&p) {
            Ok(file) => {
                let repo: Repo = serde_json::from_reader(file)?;
                if repo.name != name
                    || !valid_name(&repo.owner.project_path)
                    || repo.owner.project_id == 0
                    || !(name == repo.owner.project_path
                        || name.starts_with(&format!("{}/", repo.owner.project_path)))
                {
                    return Err(AppError::unavailable("invalid repository metadata"));
                }
                for d in repo.blobs.iter().chain(repo.manifests.keys()) {
                    check_digest(d).map_err(|_| AppError::unavailable("invalid stored digest"))?;
                }
                for (tag, value) in &repo.tags {
                    if !valid_tag(tag) || !repo.manifests.contains_key(&value.digest) {
                        return Err(AppError::unavailable("invalid stored tag"));
                    }
                }
                for manifest in repo.manifests.values() {
                    for d in &manifest.dependencies {
                        if !repo.blobs.contains(d) && !repo.manifests.contains_key(d) {
                            return Err(AppError::unavailable(
                                "incomplete manifest dependency graph",
                            ));
                        }
                    }
                }
                Ok(Some(repo))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save_repo(&self, repo: &Repo) -> Result<()> {
        self.disk.atomic(
            Path::new(&format!("repos/{}.json", repo_key(&repo.name))),
            &serde_json::to_vec(repo)?,
        )
    }
    fn writable_repo(&self, name: &str, owner: &Owner) -> Result<Repo> {
        check_name(name)?;
        if owner.project_id == 0
            || !valid_name(&owner.project_path)
            || !(name == owner.project_path
                || name.starts_with(&format!("{}/", owner.project_path)))
        {
            return Err(AppError::denied());
        }
        // Project-path binding covers every repository in a project's namespace.
        let key = format!("gc/binding-{}.json", repo_key(&owner.project_path));
        let _binding = self.disk.lock(&key, true)?;
        match self.disk.open_read(Path::new(&key)) {
            Ok(file) => {
                let previous: Owner = serde_json::from_reader(file)?;
                if previous != *owner {
                    return Err(AppError::denied());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self
                .disk
                .atomic(Path::new(&key), &serde_json::to_vec(owner)?)?,
            Err(e) => return Err(e.into()),
        }
        let repo = self.load_repo(name)?.unwrap_or_else(|| Repo {
            name: name.to_owned(),
            owner: owner.clone(),
            blobs: BTreeSet::new(),
            manifests: BTreeMap::new(),
            tags: BTreeMap::new(),
        });
        if repo.owner != *owner {
            return Err(AppError::denied());
        }
        Ok(repo)
    }
    /// Called under the publication gate. Canonical content is never partially written.
    fn install(&self, source: &Path, digest: &str) -> Result<()> {
        check_digest(digest)?;
        let target = self.disk.path(&blob_path(digest))?;
        // Upload files remain mutable until the session checkpoint is removed.
        // Never hard-link one into CAS: a crash followed by a resumed PATCH
        // would otherwise modify already published content through that inode.
        let relative = PathBuf::from(format!("incoming/.tmp-{}", uuid::Uuid::new_v4()));
        let temp = self.disk.path(&relative)?;
        let result = (|| -> Result<()> {
            let mut copy = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            std::io::copy(&mut File::open(source)?, &mut copy)?;
            copy.sync_all()?;
            match fs::hard_link(&temp, &target) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e.into()),
            }
        })();
        self.disk.remove(&relative)?;
        result?;
        let file = self.disk.open_read(&blob_path(digest))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != fs::metadata(source)?.len() {
            return Err(AppError::unavailable(
                "canonical object type or size mismatch",
            ));
        }
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
        file.sync_all()?;
        File::open(target.parent().unwrap())?.sync_all()?;
        Ok(())
    }
    pub fn content(&self, name: &str, reference: &str, manifest: bool) -> Result<Content> {
        let _gate = self.disk.lock("publication", false)?;
        let _repo = self.disk.lock(&format!("repo:{name}"), false)?;
        let repo = self.load_repo(name)?.ok_or_else(|| {
            AppError::missing(if manifest {
                "MANIFEST_UNKNOWN"
            } else {
                "BLOB_UNKNOWN"
            })
        })?;
        let (d, media_type, expected) = if manifest {
            let d = if reference.contains(':') {
                check_digest(reference)?;
                reference.to_owned()
            } else {
                if !valid_tag(reference) {
                    return Err(AppError::bad("TAG_INVALID", "invalid tag"));
                }
                repo.tags
                    .get(reference)
                    .ok_or_else(|| AppError::missing("MANIFEST_UNKNOWN"))?
                    .digest
                    .clone()
            };
            let m = repo
                .manifests
                .get(&d)
                .ok_or_else(|| AppError::missing("MANIFEST_UNKNOWN"))?;
            (d, m.media_type.clone(), Some(m.size))
        } else {
            check_digest(reference)?;
            if !repo.blobs.contains(reference) {
                return Err(AppError::missing("BLOB_UNKNOWN"));
            }
            (
                reference.to_owned(),
                "application/octet-stream".to_owned(),
                None,
            )
        };
        let file = self.disk.open_read(&blob_path(&d))?;
        let size = file.metadata()?.len();
        if expected.is_some_and(|n| n != size) {
            return Err(AppError::unavailable("stored manifest size mismatch"));
        }
        Ok(Content {
            file,
            digest: d,
            size,
            media_type,
        })
    }
    pub fn tags(&self, name: &str) -> Result<Vec<String>> {
        let _gate = self.disk.lock("publication", false)?;
        let _repo = self.disk.lock(&format!("repo:{name}"), false)?;
        Ok(self
            .load_repo(name)?
            .ok_or_else(|| AppError::missing("NAME_UNKNOWN"))?
            .tags
            .keys()
            .cloned()
            .collect())
    }
    pub fn start_upload(&self, name: &str, owner: &Owner) -> Result<Upload> {
        let _gate = self.disk.lock("publication", false)?;
        let _repo = self.disk.lock(&format!("repo:{name}"), true)?;
        let repo = self.writable_repo(name, owner)?;
        self.save_repo(&repo)?;
        let upload = Upload {
            id: uuid::Uuid::new_v4().to_string(),
            repo: name.to_owned(),
            owner: owner.clone(),
            offset: 0,
            updated_at: crate::now(),
        };
        let data = self.disk.path(&upload_path(&upload.id, "data")?)?;
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(data)?
            .sync_all()?;
        self.disk.atomic(
            &upload_path(&upload.id, "json")?,
            &serde_json::to_vec(&upload)?,
        )?;
        Ok(upload)
    }
    fn read_upload(&self, name: &str, id: &str, owner: &Owner) -> Result<Upload> {
        let path = upload_path(id, "json")?;
        let file = match self.disk.open_read(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(AppError::missing("BLOB_UPLOAD_UNKNOWN"));
            }
            Err(e) => return Err(e.into()),
        };
        let u: Upload = serde_json::from_reader(file)?;
        if u.id != id || u.repo != name || u.owner != *owner {
            return Err(AppError::missing("BLOB_UPLOAD_UNKNOWN"));
        }
        Ok(u)
    }
    pub fn upload_status(&self, name: &str, id: &str, owner: &Owner) -> Result<Upload> {
        let _lock = self.disk.lock(&format!("upload:{id}"), true)?;
        self.read_upload(name, id, owner)
    }
    pub fn cancel_upload(&self, name: &str, id: &str, owner: &Owner) -> Result<()> {
        let _lock = self.disk.lock(&format!("upload:{id}"), true)?;
        self.read_upload(name, id, owner)?;
        self.disk.remove(&upload_path(id, "json")?)?;
        self.disk.remove(&upload_path(id, "data")?)
    }
    pub fn append_upload(
        &self,
        name: &str,
        id: &str,
        owner: &Owner,
        staged: &Path,
        range: Option<(u64, u64)>,
        finish: Option<&str>,
    ) -> Result<Upload> {
        // All operations taking both locks acquire publication before upload.
        let _gate = self.disk.lock("publication", false)?;
        let _upload_lock = self.disk.lock(&format!("upload:{id}"), true)?;
        let mut upload = self.read_upload(name, id, owner)?;
        let len = fs::metadata(staged)?.len();
        if let Some((start, end)) = range
            && (start != upload.offset
                || end.checked_sub(start).and_then(|n| n.checked_add(1)) != Some(len))
        {
            return Err(AppError::new(
                axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
                "BLOB_UPLOAD_INVALID",
                "chunk range does not match committed offset",
            ));
        }
        let path = self.disk.path(&upload_path(id, "data")?)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&path)?;
        if file.metadata()?.len() < upload.offset {
            return Err(AppError::unavailable(
                "upload data shorter than committed checkpoint",
            ));
        }
        file.set_len(upload.offset)?;
        file.seek(SeekFrom::Start(upload.offset))?;
        if std::io::copy(&mut File::open(staged)?, &mut file)? != len {
            return Err(AppError::unavailable("staged chunk changed during commit"));
        }
        file.sync_all()?;
        upload.offset = upload
            .offset
            .checked_add(len)
            .ok_or_else(|| AppError::bad("SIZE_INVALID", "upload too large"))?;
        upload.updated_at = crate::now();
        self.disk
            .atomic(&upload_path(id, "json")?, &serde_json::to_vec(&upload)?)?;
        if let Some(expected) = finish {
            check_digest(expected)?;
            file.seek(SeekFrom::Start(0))?;
            let mut hash = Sha256::new();
            let mut buf = [0u8; 128 * 1024];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            if format!("sha256:{}", hex::encode(hash.finalize())) != expected {
                return Err(AppError::bad(
                    "DIGEST_INVALID",
                    "uploaded content does not match digest",
                ));
            }
            let _repo = self.disk.lock(&format!("repo:{name}"), true)?;
            let mut repo = self.writable_repo(name, owner)?;
            self.install(&path, expected)?;
            repo.blobs.insert(expected.to_owned());
            self.save_repo(&repo)?;
            self.disk.remove(&upload_path(id, "json")?)?;
            self.disk.remove(&upload_path(id, "data")?)?;
        }
        Ok(upload)
    }
    pub fn mount(&self, name: &str, from: &str, d: &str, owner: &Owner) -> Result<bool> {
        check_name(from)?;
        check_digest(d)?;
        let _gate = self.disk.lock("publication", false)?;
        // Source links are only removed by GC under an exclusive gate, so no second repo lock needed.
        if !self.load_repo(from)?.is_some_and(|r| r.blobs.contains(d)) {
            return Ok(false);
        }
        let _repo = self.disk.lock(&format!("repo:{name}"), true)?;
        let mut repo = self.writable_repo(name, owner)?;
        let file = self.disk.open_read(&blob_path(d))?;
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
        file.sync_all()?;
        repo.blobs.insert(d.to_owned());
        self.save_repo(&repo)?;
        Ok(true)
    }
    pub fn put_manifest(
        &self,
        name: &str,
        reference: &str,
        owner: &Owner,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<String> {
        if bytes.len() > MAX_MANIFEST {
            return Err(AppError::bad("MANIFEST_INVALID", "manifest too large"));
        }
        let d = digest(bytes);
        if reference.contains(':') {
            check_digest(reference)?;
            if reference != d {
                return Err(AppError::bad("DIGEST_INVALID", "manifest digest mismatch"));
            }
        } else if !valid_tag(reference) {
            return Err(AppError::bad("TAG_INVALID", "invalid tag"));
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|_| AppError::bad("MANIFEST_INVALID", "invalid manifest JSON"))?;
        if value.get("schemaVersion").and_then(|v| v.as_u64()) != Some(2)
            || !matches!(
                media_type,
                OCI_MANIFEST | OCI_INDEX | DOCKER_MANIFEST | DOCKER_INDEX
            )
            || value
                .get("mediaType")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s != media_type)
        {
            return Err(AppError::bad(
                "MANIFEST_INVALID",
                "unsupported or inconsistent manifest type/schema",
            ));
        }
        let index = matches!(media_type, OCI_INDEX | DOCKER_INDEX);
        let descriptors: Vec<&serde_json::Value> = if index {
            value
                .get("manifests")
                .and_then(|v| v.as_array())
                .ok_or_else(|| AppError::bad("MANIFEST_INVALID", "missing manifests"))?
                .iter()
                .collect()
        } else {
            let mut v = vec![
                value
                    .get("config")
                    .ok_or_else(|| AppError::bad("MANIFEST_INVALID", "missing config"))?,
            ];
            v.extend(
                value
                    .get("layers")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| AppError::bad("MANIFEST_INVALID", "missing layers"))?,
            );
            v
        };
        let _gate = self.disk.lock("publication", false)?;
        let _repo = self.disk.lock(&format!("repo:{name}"), true)?;
        let mut repo = self.writable_repo(name, owner)?;
        let mut dependencies = Vec::new();
        for desc in descriptors {
            let dep = desc
                .get("digest")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AppError::bad("MANIFEST_INVALID", "descriptor missing digest"))?;
            check_digest(dep)?;
            let size = desc
                .get("size")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| AppError::bad("MANIFEST_INVALID", "descriptor missing size"))?;
            if desc.get("mediaType").and_then(|v| v.as_str()).is_none() {
                return Err(AppError::bad(
                    "MANIFEST_INVALID",
                    "descriptor missing mediaType",
                ));
            }
            if if index {
                !repo.manifests.contains_key(dep)
            } else {
                !repo.blobs.contains(dep)
            } {
                return Err(AppError::bad(
                    "MANIFEST_BLOB_UNKNOWN",
                    "manifest dependency is not in this repository",
                ));
            }
            if self.disk.open_read(&blob_path(dep))?.metadata()?.len() != size {
                return Err(AppError::bad(
                    "MANIFEST_INVALID",
                    "descriptor size mismatch",
                ));
            }
            dependencies.push(dep.to_owned());
        }
        // Subject references are discovery metadata, not ownership of the subject's lifetime.
        if let Some(subject) = value.get("subject") {
            let dep = subject
                .get("digest")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AppError::bad("MANIFEST_INVALID", "invalid subject"))?;
            check_digest(dep)?;
        }
        // Renew the grace clock for dependencies even if a tag is subsequently
        // deleted before the next GC scan observes that it was reachable.
        let mut pending = dependencies.clone();
        let mut renewed = BTreeSet::new();
        while let Some(dep) = pending.pop() {
            if renewed.insert(dep.clone()) {
                let file = self.disk.open_read(&blob_path(&dep))?;
                file.set_times(
                    std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                )?;
                file.sync_all()?;
                if let Some(child) = repo.manifests.get(&dep) {
                    pending.extend(child.dependencies.iter().cloned());
                }
            }
        }
        let relative = PathBuf::from(format!("incoming/{}", uuid::Uuid::new_v4()));
        self.disk.atomic(&relative, bytes)?;
        let staged = self.disk.path(&relative)?;
        let install = self.install(&staged, &d);
        self.disk.remove(&relative)?;
        install?;
        repo.manifests.insert(
            d.clone(),
            Manifest {
                media_type: media_type.to_owned(),
                size: bytes.len() as u64,
                dependencies,
            },
        );
        if !reference.contains(':') {
            repo.tags.insert(
                reference.to_owned(),
                Tag {
                    digest: d.clone(),
                    updated_at: crate::now(),
                },
            );
        }
        self.save_repo(&repo)?;
        Ok(d)
    }
    pub fn delete_manifest(&self, name: &str, reference: &str, owner: &Owner) -> Result<()> {
        let _gate = self.disk.lock("publication", false)?;
        let _repo = self.disk.lock(&format!("repo:{name}"), true)?;
        let mut repo = self.writable_repo(name, owner)?;
        if reference.contains(':') {
            check_digest(reference)?;
            if !repo.manifests.contains_key(reference) {
                return Err(AppError::missing("MANIFEST_UNKNOWN"));
            }
            // Preserve dependencies of other manifests rather than leaving corrupt indexes.
            if repo
                .manifests
                .values()
                .any(|m| m.dependencies.iter().any(|d| d == reference))
            {
                return Err(AppError::bad(
                    "MANIFEST_INVALID",
                    "manifest is referenced by another manifest",
                ));
            }
            repo.tags.retain(|_, t| t.digest != reference);
            repo.manifests.remove(reference);
        } else {
            if !valid_tag(reference) {
                return Err(AppError::bad("TAG_INVALID", "invalid tag"));
            }
            if repo.tags.remove(reference).is_none() {
                return Err(AppError::missing("MANIFEST_UNKNOWN"));
            }
        }
        self.save_repo(&repo)
    }
}

#[cfg(test)]
mod crash_tests {
    use super::*;
    #[test]
    fn published_content_does_not_alias_resumable_upload() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let source = root.path().join("incoming/upload");
        fs::write(&source, b"original").unwrap();
        let d = digest(b"original");
        store.install(&source, &d).unwrap();
        fs::write(&source, b"resumed upload after interrupted finalization").unwrap();
        assert_eq!(store.disk.read(&blob_path(&d)).unwrap(), b"original");
    }
}
