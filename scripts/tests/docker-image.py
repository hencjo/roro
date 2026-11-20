#!/usr/bin/env python3
"""Create a five-layer scratch context; modern-store tests import its Docker archive."""
import hashlib
import io
import json
import os
from pathlib import Path
import sys
import tarfile

root = Path(sys.argv[1])
size_mib = int(sys.argv[2])
for i in range(5):
    with (root / f"layer{i}").open("wb") as output:
        for _ in range(size_mib):
            output.write(os.urandom(1024 * 1024))
(root / "Dockerfile").write_text("FROM scratch\n" + "".join(f"COPY layer{i} /layer{i}\n" for i in range(5)))
if sys.argv[4] == "containerd":
    layers = []
    digests = []
    for i in range(5):
        name = f"{i}/layer.tar"
        path = root / name
        path.parent.mkdir()
        with tarfile.open(path, "w", format=tarfile.USTAR_FORMAT) as archive:
            info = tarfile.TarInfo(f"layer{i}")
            info.size = (root / f"layer{i}").stat().st_size
            info.mode = 0o644
            with (root / f"layer{i}").open("rb") as source:
                archive.addfile(info, source)
        with path.open("rb") as source:
            digests.append("sha256:" + hashlib.file_digest(source, "sha256").hexdigest())
        layers.append(name)
    config = json.dumps({"architecture": "amd64", "os": "linux", "config": {},
                         "rootfs": {"type": "layers", "diff_ids": digests},
                         "history": [{"created_by": "roro acceptance COPY"} for _ in layers]}).encode()
    name = hashlib.sha256(config).hexdigest() + ".json"
    manifest = json.dumps([{"Config": name, "RepoTags": [sys.argv[3]], "Layers": layers}]).encode()
    with tarfile.open(root / "image.tar", "w") as archive:
        for filename, data in [(name, config), ("manifest.json", manifest)]:
            info = tarfile.TarInfo(filename)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        for layer in layers:
            archive.add(root / layer, arcname=layer, recursive=False)
