//! All callers use persistent lock inodes. Never unlink or rename a lock file.
use crate::error::{AppError, Result};
use rustix::fs::{FlockOperation, flock};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub struct Lock {
    _file: File,
}
#[derive(Clone)]
pub struct Disk {
    pub root: PathBuf,
    pub lock_waits: Arc<AtomicU64>,
}
impl Disk {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(AppError::bad(
                "CONFIG_INVALID",
                "dataRoot must be an absolute path without parent traversal",
            ));
        }
        // Refuse symlink components, including ancestors of the configured root.
        let mut path = PathBuf::new();
        for component in root.components() {
            path.push(component);
            if let Ok(meta) = fs::symlink_metadata(&path)
                && meta.file_type().is_symlink()
            {
                return Err(AppError::bad(
                    "CONFIG_INVALID",
                    "dataRoot must not contain symlinks",
                ));
            }
        }
        fs::create_dir_all(root)?;
        if let Some(parent) = root.parent() {
            File::open(parent)?.sync_all()?;
        }
        let disk = Self {
            root: root.to_path_buf(),
            lock_waits: Arc::default(),
        };
        for dir in ["locks", "blobs", "repos", "uploads", "incoming", "gc"] {
            disk.mkdir(Path::new(dir))?;
        }
        use std::os::unix::fs::MetadataExt;
        let device = fs::metadata(root)?.dev();
        for dir in ["locks", "blobs", "repos", "uploads", "incoming", "gc"] {
            if fs::metadata(root.join(dir))?.dev() != device {
                return Err(AppError::bad(
                    "CONFIG_INVALID",
                    "all storage directories must be on one filesystem",
                ));
            }
        }
        disk.check_mount()?;
        // A local probe detects broken locking, not cross-host correctness.
        let _probe = disk.lock("startup-probe", true)?;
        if disk.try_lock("startup-probe", true)?.is_some() {
            return Err(AppError::unavailable(
                "filesystem did not enforce advisory locks",
            ));
        }
        Ok(disk)
    }
    pub fn path(&self, relative: &Path) -> Result<PathBuf> {
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(AppError::bad("NAME_INVALID", "invalid storage path"));
        }
        let mut path = self.root.clone();
        for part in relative.components() {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(AppError::unavailable("symlink found in storage"));
                }
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(path)
    }
    pub fn mkdir(&self, relative: &Path) -> Result<()> {
        let path = self.path(relative)?;
        fs::create_dir_all(&path)?;
        // Synchronize every created directory entry up to the root.
        let mut current = path.as_path();
        while current.starts_with(&self.root) {
            File::open(current)?.sync_all()?;
            if current == self.root {
                break;
            }
            current = current.parent().unwrap();
        }
        Ok(())
    }
    fn lock_file(&self, name: &str) -> Result<File> {
        let hash = hex::encode(Sha256::digest(name.as_bytes()));
        // Bound lock-inode growth for transient UUIDs. Collisions only serialize
        // unrelated work; lock classes are separate to preserve acquisition order.
        let key = if name.starts_with("upload:") {
            format!("upload-{}", &hash[..3])
        } else if name.starts_with("upload-active:") {
            format!("active-{}", &hash[..3])
        } else if name.starts_with("incoming:") {
            format!("incoming-{}", &hash[..3])
        } else {
            hash
        };
        let p = self.path(Path::new(&format!("locks/{key}")))?;
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(p)?)
    }
    pub fn try_lock(&self, name: &str, exclusive: bool) -> Result<Option<Lock>> {
        let file = self.lock_file(name)?;
        match flock(
            &file,
            if exclusive {
                FlockOperation::NonBlockingLockExclusive
            } else {
                FlockOperation::NonBlockingLockShared
            },
        ) {
            Ok(()) => Ok(Some(Lock { _file: file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(AppError::internal(e)),
        }
    }
    pub fn lock(&self, name: &str, exclusive: bool) -> Result<Lock> {
        let start = Instant::now();
        loop {
            if let Some(lock) = self.try_lock(name, exclusive)? {
                return Ok(lock);
            }
            self.lock_waits.fetch_add(1, Ordering::Relaxed);
            if start.elapsed() > Duration::from_secs(30) {
                return Err(AppError::unavailable("storage lock timeout; retry request"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn read(&self, relative: &Path) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_read(relative)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    pub fn open_read(&self, relative: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(self.path(relative).map_err(std::io::Error::other)?)
    }
    pub fn atomic(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        let target = self.path(relative)?;
        let parent = target.parent().unwrap();
        let temp = parent.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temp, &target)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
    pub fn remove(&self, relative: &Path) -> Result<()> {
        let p = self.path(relative)?;
        match fs::remove_file(&p) {
            Ok(()) => File::open(p.parent().unwrap())?.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    fn check_mount(&self) -> Result<()> {
        let mounts = fs::read_to_string("/proc/mounts")?;
        let mut selected: Option<(usize, &str, &str)> = None;
        for line in mounts.lines() {
            let f: Vec<_> = line.split_whitespace().collect();
            if f.len() < 4 {
                continue;
            }
            let mount = f[1].replace("\\040", " ");
            if self.root.starts_with(&mount) && selected.is_none_or(|s| mount.len() > s.0) {
                selected = Some((mount.len(), f[2], f[3]));
            }
        }
        if let Some((_, kind, opts)) = selected {
            if kind == "nfs" || kind == "nfs4" {
                let options: Vec<_> = opts.split(',').collect();
                if !options.iter().any(|o| o.starts_with("vers=4"))
                    || options.iter().any(|o| {
                        matches!(
                            *o,
                            "soft"
                                | "softerr"
                                | "softreval"
                                | "nolock"
                                | "local_lock=all"
                                | "local_lock=flock"
                        )
                    })
                {
                    return Err(AppError::bad(
                        "CONFIG_INVALID",
                        "NFS requires v4, hard mounts and cross-client file locks",
                    ));
                }
            } else if kind.starts_with("fuse") || matches!(kind, "cifs" | "smb3" | "9p") {
                return Err(AppError::bad(
                    "CONFIG_INVALID",
                    "unsupported shared filesystem",
                ));
            }
        }
        Ok(())
    }
}
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(AppError::internal)?
}
