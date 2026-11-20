use roro::{
    fs::Disk,
    gc::{DAY, GRACE, run_at},
    store::{OCI_INDEX, OCI_MANIFEST, Owner, Store, blob_path, digest, upload_path},
};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
fn setup() -> (tempfile::TempDir, Store, Owner) {
    let temp = if let Ok(root) = std::env::var("DISK_REGISTRY_TEST_ROOT") {
        tempfile::tempdir_in(root).unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    let s = Store::open(temp.path()).unwrap();
    (temp, s, owner())
}
fn owner() -> Owner {
    Owner {
        project_id: 12,
        project_path: "team/project".into(),
    }
}
fn blob(s: &Store, o: &Owner, name: &str, bytes: &[u8]) -> String {
    let u = s.start_upload(name, o).unwrap();
    let path = s
        .disk
        .root
        .join(format!("incoming/{}", uuid::Uuid::new_v4()));
    fs::write(&path, bytes).unwrap();
    let d = digest(bytes);
    s.append_upload(name, &u.id, o, &path, None, Some(&d))
        .unwrap();
    fs::remove_file(path).unwrap();
    d
}
fn manifest(s: &Store, o: &Owner, name: &str, tag: &str, bytes: &[u8]) -> String {
    let d = blob(s, o, name, bytes);
    let body=serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"mediaType":OCI_MANIFEST,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":d,"size":bytes.len()},"layers":[]})).unwrap();
    s.put_manifest(name, tag, o, OCI_MANIFEST, &body).unwrap()
}
#[test]
fn names_and_digests() {
    for n in ["team/project", "a_b/c__d/e--f", "a.b/a0"] {
        assert!(roro::store::valid_name(n));
    }
    for n in [
        "../etc",
        "/absolute",
        "team//app",
        "a/../app",
        "UPPER/app",
        "a_",
        "a...b",
        "a___b",
        "a/.x",
    ] {
        assert!(!roro::store::valid_name(n));
    }
    assert!(roro::store::check_digest(&digest(b"x")).is_ok());
    assert!(roro::store::check_digest("sha256:abcd").is_err());
}
#[test]
fn upload_resume_and_uncommitted_tail_recovery() {
    let (_t, s, o) = setup();
    let name = "team/project";
    let u = s.start_upload(name, &o).unwrap();
    let chunk = s.disk.root.join("incoming/chunk");
    fs::write(&chunk, b"abc").unwrap();
    s.append_upload(name, &u.id, &o, &chunk, Some((0, 2)), None)
        .unwrap();
    let data = s.disk.path(&upload_path(&u.id, "data").unwrap()).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(data)
        .unwrap()
        .write_all(b"uncommitted")
        .unwrap();
    let other = Store::open(&s.disk.root).unwrap();
    assert_eq!(other.upload_status(name, &u.id, &o).unwrap().offset, 3);
    fs::write(&chunk, b"def").unwrap();
    let d = digest(b"abcdef");
    other
        .append_upload(name, &u.id, &o, &chunk, Some((3, 5)), Some(&d))
        .unwrap();
    let mut c = s.content(name, &d, false).unwrap();
    let mut bytes = Vec::new();
    c.file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"abcdef");
    assert!(s.upload_status(name, &u.id, &o).is_err());
}
#[test]
fn invalid_range_and_digest_do_not_publish() {
    let (_t, s, o) = setup();
    let u = s.start_upload("team/project", &o).unwrap();
    let p = s.disk.root.join("incoming/chunk");
    fs::write(&p, b"abc").unwrap();
    assert!(
        s.append_upload("team/project", &u.id, &o, &p, Some((1, 3)), None)
            .is_err()
    );
    assert_eq!(
        s.upload_status("team/project", &u.id, &o).unwrap().offset,
        0
    );
    let bad = digest(b"wrong");
    assert!(
        s.append_upload("team/project", &u.id, &o, &p, None, Some(&bad))
            .is_err()
    );
    assert!(s.content("team/project", &bad, false).is_err());
    assert_eq!(
        s.upload_status("team/project", &u.id, &o).unwrap().offset,
        3
    );
}
#[test]
fn project_binding_prevents_takeover_across_repositories() {
    let (_t, s, o) = setup();
    s.start_upload("team/project/a", &o).unwrap();
    let mut impostor = o.clone();
    impostor.project_id = 99;
    assert!(s.start_upload("team/project/b", &impostor).is_err());
    assert!(s.start_upload("team/project-other", &o).is_err());
    assert!(s.start_upload("other/project", &o).is_err());
}
#[test]
fn manifest_validation_mount_and_dependency_deletion() {
    let (_t, s, o) = setup();
    let d = manifest(&s, &o, "team/project", "v1", b"{}");
    assert!(
        s.mount("team/project/sub", "team/project", &digest(b"{}"), &o)
            .unwrap()
    );
    let index=serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"mediaType":OCI_INDEX,"manifests":[{"mediaType":OCI_MANIFEST,"digest":d,"size":s.content("team/project",&d,true).unwrap().size}]})).unwrap();
    let idx = s
        .put_manifest("team/project", "latest", &o, OCI_INDEX, &index)
        .unwrap();
    assert!(s.delete_manifest("team/project", &d, &o).is_err());
    s.delete_manifest("team/project", &idx, &o).unwrap();
    s.delete_manifest("team/project", &d, &o).unwrap();
    assert!(
        s.put_manifest("team/project", "bad", &o, OCI_MANIFEST, b"{}")
            .is_err()
    );
    assert!(
        s.put_manifest("team/project", &digest(b"wrong"), &o, OCI_INDEX, &index)
            .is_err()
    );
}
#[test]
fn retention_grace_graphs_and_open_downloads() {
    let (_t, s, o) = setup();
    let now = roro::now();
    let keep = manifest(&s, &o, "team/project", "latest", b"keep");
    let gone = manifest(&s, &o, "team/project", "old", b"gone");
    let now = now.max(roro::now());
    let mut open = s.content("team/project", &gone, true).unwrap();
    assert_eq!(run_at(&s, now).unwrap().expired_tags, 0);
    assert_eq!(run_at(&s, now + 30 * DAY).unwrap().expired_tags, 1);
    assert!(s.content("team/project", &gone, true).is_ok());
    let report = run_at(&s, now + 30 * DAY + GRACE).unwrap();
    assert!(report.removed_objects >= 2);
    assert!(s.content("team/project", &keep, true).is_ok());
    assert!(s.content("team/project", &gone, true).is_err());
    let mut bytes = Vec::new();
    open.file.read_to_end(&mut bytes).unwrap();
    assert!(!bytes.is_empty());
}
#[test]
fn tag_refresh_and_resurrection() {
    let (_t, s, o) = setup();
    let now = roro::now();
    let d = manifest(&s, &o, "team/project", "v1", b"data");
    let now = now.max(roro::now());
    run_at(&s, now + 30 * DAY).unwrap();
    assert!(s.tags("team/project").unwrap().is_empty());
    manifest(&s, &o, "team/project", "latest", b"data");
    run_at(&s, now + 40 * DAY).unwrap();
    assert!(s.content("team/project", &d, true).is_ok());
}
#[test]
fn multiarch_children_are_retained() {
    let (_t, s, o) = setup();
    let now = roro::now();
    let d = manifest(&s, &o, "team/project", "temp", b"child");
    let size = s.content("team/project", &d, true).unwrap().size;
    let b=serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"mediaType":OCI_INDEX,"manifests":[{"mediaType":OCI_MANIFEST,"digest":d,"size":size}]})).unwrap();
    s.put_manifest("team/project", "latest", &o, OCI_INDEX, &b)
        .unwrap();
    run_at(&s, now + 31 * DAY).unwrap();
    run_at(&s, now + 50 * DAY).unwrap();
    assert!(s.content("team/project", &d, true).is_ok());
    assert!(s.content("team/project", &digest(b"child"), false).is_ok());
}
#[test]
fn corruption_prevents_sweep() {
    let (_t, s, o) = setup();
    let now = roro::now();
    let d = blob(&s, &o, "team/project", b"orphan");
    run_at(&s, now).unwrap();
    fs::write(s.disk.root.join("repos/broken.json"), b"not json").unwrap();
    assert!(run_at(&s, now + 10 * DAY).is_err());
    assert!(s.disk.path(&blob_path(&d)).unwrap().exists());
}
#[test]
fn concurrent_publish_gc_and_duplicate_content() {
    let (_t, s, o) = setup();
    let s = Arc::new(s);
    let mut threads = Vec::new();
    for i in 0..8 {
        let s = s.clone();
        let o = o.clone();
        threads.push(std::thread::spawn(move || {
            for _ in 0..5 {
                manifest(&s, &o, "team/project", &format!("v{i}"), b"shared");
            }
        }));
    }
    let gc_s = s.clone();
    let gc = std::thread::spawn(move || {
        for _ in 0..10 {
            run_at(&gc_s, roro::now()).unwrap();
        }
    });
    for t in threads {
        t.join().unwrap();
    }
    gc.join().unwrap();
    assert_eq!(s.tags("team/project").unwrap().len(), 8);
    assert_eq!(fs::read_dir(s.disk.root.join("blobs")).unwrap().count(), 2);
}
#[test]
fn rejects_symlink_escape() {
    let t = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/tmp", t.path().join("blobs")).unwrap();
    assert!(Store::open(t.path()).is_err());
    let (_t, s, _) = setup();
    std::os::unix::fs::symlink("/etc/passwd", s.disk.root.join("incoming/escape")).unwrap();
    assert!(s.disk.open_read(Path::new("incoming/escape")).is_err());
}
#[test]
fn locks_exclude_independent_opens() {
    let (_t, s, _) = setup();
    let shared = s.disk.lock("test", false).unwrap();
    assert!(s.disk.try_lock("test", true).unwrap().is_none());
    assert!(s.disk.try_lock("test", false).unwrap().is_some());
    drop(shared);
    // A concurrently spawned subprocess can briefly inherit an open description
    // before exec closes CLOEXEC descriptors; allow the normal bounded retry.
    let _exclusive = s.disk.lock("test", true).unwrap();
}
#[test]
fn process_death_releases_lock() {
    let (_t, s, _) = setup();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "lock_holder_child"])
        .env("DR_CHILD_ROOT", &s.disk.root)
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !s.disk.root.join("holder-ready").exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.disk.try_lock("child-lock", true).unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(s.disk.try_lock("child-lock", true).unwrap().is_some());
}
#[test]
#[ignore]
fn lock_holder_child() {
    let Ok(root) = std::env::var("DR_CHILD_ROOT") else {
        return;
    };
    let disk = Disk::open(Path::new(&root)).unwrap();
    let _lock = disk.lock("child-lock", true).unwrap();
    fs::write(Path::new(&root).join("holder-ready"), b"ready").unwrap();
    std::thread::sleep(Duration::from_secs(120));
}
#[test]
#[ignore]
fn cross_host_holder() {
    let root = std::env::var("DR_PROBE_ROOT")
        .expect("set DR_PROBE_ROOT to a dedicated shared test directory");
    let disk = Disk::open(Path::new(&root)).unwrap();
    let _lock = disk.lock("cross-host-probe", true).unwrap();
    fs::write(Path::new(&root).join("probe-ready"), b"ready").unwrap();
    let start = Instant::now();
    while !Path::new(&root).join("probe-release").exists() {
        assert!(start.elapsed() < Duration::from_secs(120));
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[test]
#[ignore]
fn cross_host_contender() {
    let root = std::env::var("DR_PROBE_ROOT").expect("set DR_PROBE_ROOT");
    let disk = Disk::open(Path::new(&root)).unwrap();
    assert!(
        Path::new(&root).join("probe-ready").exists(),
        "start holder first"
    );
    assert!(
        disk.try_lock("cross-host-probe", true).unwrap().is_none(),
        "BROKEN cross-host locks"
    );
    fs::write(Path::new(&root).join("probe-release"), b"release").unwrap();
    let _lock = disk.lock("cross-host-probe", true).unwrap();
}

#[test]
fn corrupt_manifest_or_dependencies_aborts_gc() {
    let (_temp, store, owner) = setup();
    let d = manifest(&store, &owner, "team/project", "latest", b"config");
    let mut repo = store.load_repo("team/project").unwrap().unwrap();
    repo.manifests.get_mut(&d).unwrap().dependencies.clear();
    store.save_repo(&repo).unwrap();
    assert!(run_at(&store, roro::now() + 100 * DAY).is_err());
    assert!(store.disk.open_read(&blob_path(&d)).is_ok());
}

#[test]
fn upload_cleanup_and_coordinator_exclusion() {
    let (_temp, store, owner) = setup();
    let now = roro::now();
    let upload = store.start_upload("team/project", &owner).unwrap();
    let lease = store
        .disk
        .lock(&format!("upload:{}", upload.id), true)
        .unwrap();
    run_at(&store, now + 2 * DAY).unwrap();
    assert!(
        store
            .disk
            .open_read(&upload_path(&upload.id, "json").unwrap())
            .is_ok()
    );
    drop(lease);
    let coordinator = store.disk.lock("gc-coordinator", true).unwrap();
    assert_eq!(run_at(&store, now + 2 * DAY).unwrap().removed_uploads, 0);
    drop(coordinator);
    assert_eq!(run_at(&store, now + 2 * DAY).unwrap().removed_uploads, 1);
    assert!(
        store
            .upload_status("team/project", &upload.id, &owner)
            .is_err()
    );
}

#[test]
fn tag_age_boundary_and_timestamp_refresh() {
    let (_temp, store, owner) = setup();
    manifest(&store, &owner, "team/project", "v1", b"data");
    let mut repo = store.load_repo("team/project").unwrap().unwrap();
    let now = roro::now();
    repo.tags.get_mut("v1").unwrap().updated_at = now - 30 * DAY + 1;
    store.save_repo(&repo).unwrap();
    assert_eq!(run_at(&store, now).unwrap().expired_tags, 0);
    assert_eq!(run_at(&store, now + 1).unwrap().expired_tags, 1);
    manifest(&store, &owner, "team/project", "v1", b"data");
    assert_eq!(run_at(&store, roro::now()).unwrap().expired_tags, 0);
}

#[test]
fn interrupted_atomic_replacement_is_whole() {
    let (_temp, store, _) = setup();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "atomic_writer_child"])
        .env("DR_CHILD_ROOT", &store.disk.root)
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !store.disk.root.join("writer-ready").exists() {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(50));
    child.kill().unwrap();
    child.wait().unwrap();
    let bytes = store
        .disk
        .read(Path::new("gc/crash-snapshot.json"))
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["payload"].as_str().unwrap().len(), 65536);
}

#[test]
#[ignore]
fn atomic_writer_child() {
    let Ok(root) = std::env::var("DR_CHILD_ROOT") else {
        return;
    };
    let disk = Disk::open(Path::new(&root)).unwrap();
    for generation in 0u64.. {
        let bytes = serde_json::to_vec(
            &serde_json::json!({"generation": generation, "payload": "x".repeat(65536)}),
        )
        .unwrap();
        disk.atomic(Path::new("gc/crash-snapshot.json"), &bytes)
            .unwrap();
        if generation == 0 {
            fs::write(Path::new(&root).join("writer-ready"), b"ready").unwrap();
        }
    }
}

#[test]
fn active_transfer_lease_prevents_session_expiry() {
    let (_temp, store, owner) = setup();
    let upload = store.start_upload("team/project", &owner).unwrap();
    let active = store
        .disk
        .lock(&format!("upload-active:{}", upload.id), true)
        .unwrap();
    assert_eq!(
        run_at(&store, roro::now() + 2 * DAY)
            .unwrap()
            .removed_uploads,
        0
    );
    drop(active);
    assert_eq!(
        run_at(&store, roro::now() + 2 * DAY)
            .unwrap()
            .removed_uploads,
        1
    );
}
