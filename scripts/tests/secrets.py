#!/usr/bin/env python3
"""Exercise secret gates using generated fake credentials in disposable Git repos."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
ENV = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
           GIT_AUTHOR_NAME="Scanner Test", GIT_AUTHOR_EMAIL="scanner@example.invalid",
           GIT_COMMITTER_NAME="Scanner Test", GIT_COMMITTER_EMAIL="scanner@example.invalid")


def git(repo, *args):
    subprocess.run(["git", "-C", str(repo), *args], env=ENV, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def scan(repo, accepted):
    result = subprocess.run(["python3", "-B", "scripts/check-secrets"], cwd=repo,
                            env=ENV, capture_output=True, text=True)
    assert (result.returncode == 0) == accepted, result.stdout + result.stderr
    assert credential not in result.stdout + result.stderr, "Scanner leaked credential"


credential = "ghp_" + hashlib.sha256(b"roro generated scanner test").hexdigest()[:36]
with tempfile.TemporaryDirectory(prefix="roro-scanner-test-") as directory:
    repo = Path(directory)
    for name in ("scripts/check-secrets", ".gitleaks.toml", "tests/fixtures/ci-test-private.pem"):
        target = repo / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / name, target)
    git(repo, "init", "-q", "-b", "main")
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "test: clean public fixture")
    scan(repo, True)
    leak = repo / "credential.txt"
    leak.write_text("github_token=" + credential + "\n")
    scan(repo, False)  # Untracked files are publishable too.
    git(repo, "add", ".")
    scan(repo, False)
    git(repo, "commit", "-qm", "test: synthetic credential")
    git(repo, "rm", "-q", "credential.txt")
    git(repo, "commit", "-qm", "test: remove credential")
    scan(repo, False)  # Deletion cannot hide a historical leak.
    git(repo, "checkout", "--orphan", "clean")
    git(repo, "commit", "-qm", "test: clean branch")
    scan(repo, False)  # All refs, not only HEAD.
    git(repo, "branch", "-D", "main")
    scan(repo, True)
    key = repo / "tests/fixtures/ci-test-private.pem"
    (repo / "another-private.pem").write_bytes(key.read_bytes())
    scan(repo, False)  # Public-key exemption is path-specific.
    (repo / "another-private.pem").unlink()
    original = key.read_bytes()
    key.write_bytes(original + b"\n")
    scan(repo, False)  # Exempted fixture bytes are pinned.
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "test: changed fixture")
    key.write_bytes(original)
    git(repo, "add", ".")
    git(repo, "commit", "-qm", "test: restored fixture")
    scan(repo, False)  # Historical fixture changes also fail.
print("Secret scanning: clean, untracked, staged, historical/all-ref, redaction and fixture guards passed.")
