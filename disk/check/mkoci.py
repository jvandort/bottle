"""OCI archives for checking bottle-disk (see check.sh).

    mkoci.py prefix IN OUT K      OUT: IN's image cut to its first K layers (a base of IN)
    mkoci.py synth OUT            OUT: a synthetic image of the cases bottle-disk must get right
"""
import gzip, hashlib, io, json, sys, tarfile, time


def sha(b):
    return "sha256:" + hashlib.sha256(b).hexdigest()


def write_archive(out, layers, config_extra=None):
    """layers: (compressed blob, media type, diff_id)."""
    blobs = {}
    config = {"architecture": "arm64", "os": "linux",
              "rootfs": {"type": "layers", "diff_ids": [d for _, _, d in layers]}}
    cb = json.dumps(config).encode()
    blobs[sha(cb)] = cb
    manifest = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                           "digest": sha(cb), "size": len(cb)},
                "layers": []}
    for blob, mt, _ in layers:
        blobs[sha(blob)] = blob
        manifest["layers"].append({"mediaType": mt, "digest": sha(blob), "size": len(blob)})
    mb = json.dumps(manifest).encode()
    blobs[sha(mb)] = mb
    index = {"schemaVersion": 2, "manifests": [
        {"mediaType": manifest["mediaType"], "digest": sha(mb), "size": len(mb)}]}
    with tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as t:
        def add(name, data):
            ti = tarfile.TarInfo(name)
            ti.size = len(data)
            t.addfile(ti, io.BytesIO(data))
        add("oci-layout", b'{"imageLayoutVersion":"1.0.0"}')
        add("index.json", json.dumps(index).encode())
        for d, b in blobs.items():
            add("blobs/sha256/" + d.split(":")[1], b)


def read_archive(path):
    t = tarfile.open(path)
    get = lambda n: t.extractfile(n).read()
    index = json.loads(get("index.json"))
    m = json.loads(get("blobs/sha256/" + index["manifests"][0]["digest"].split(":")[1]))
    c = json.loads(get("blobs/sha256/" + m["config"]["digest"].split(":")[1]))
    return [(get("blobs/sha256/" + l["digest"].split(":")[1]), l["mediaType"], d)
            for l, d in zip(m["layers"], c["rootfs"]["diff_ids"])]


def layer(entries):
    """entries: (name, kind, extra dict). kind: dir file link sym chr blk fifo."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as t:
        for name, kind, x in entries:
            ti = tarfile.TarInfo(name)
            ti.mtime = x.get("mtime", 1700000000)
            ti.uid = x.get("uid", 0)
            ti.gid = x.get("gid", 0)
            ti.mode = x.get("mode", 0o755 if kind == "dir" else 0o644)
            if "xattrs" in x:
                ti.pax_headers = {"SCHILY.xattr." + k: v for k, v in x["xattrs"].items()}
            data = None
            if kind == "dir":
                ti.type = tarfile.DIRTYPE
            elif kind == "file":
                data = x.get("data", name.encode() + b"\n")
                ti.size = len(data)
            elif kind == "link":
                ti.type = tarfile.LNKTYPE
                ti.linkname = x["to"]
            elif kind == "sym":
                ti.type = tarfile.SYMTYPE
                ti.linkname = x["to"]
            elif kind in ("chr", "blk"):
                ti.type = tarfile.CHRTYPE if kind == "chr" else tarfile.BLKTYPE
                ti.devmajor, ti.devminor = x["dev"]
            elif kind == "fifo":
                ti.type = tarfile.FIFOTYPE
            t.addfile(ti, io.BytesIO(data) if data is not None else None)
    raw = buf.getvalue()
    return (gzip.compress(raw, mtime=0), "application/vnd.oci.image.layer.v1.tar+gzip", sha(raw))


D, F = "dir", "file"
LONG = "deep/" + "/".join(["d" * 60] * 3)


def synth():
    return [
        # 0: a base
        layer([("./", D, {"mode": 0o755}), ("etc", D, {}), ("etc/a", F, {}), ("etc/b", F, {}),
               ("etc/sub", D, {}), ("etc/sub/x", F, {}), ("etc/sub/deeper", D, {}),
               ("etc/sub/deeper/y", F, {}),
               ("opq", D, {}), ("opq/keep", F, {}), ("opq/gone", F, {}), ("opq/d", D, {}),
               ("opq/d/inner", F, {}), ("opq/d2", D, {}), ("opq/d2/z", F, {}),
               ("f2d", F, {}), ("d2f", D, {}), ("d2f/child", F, {}),
               ("bin", D, {}), ("bin/tool", F, {"mode": 0o4755, "uid": 0,
                "xattrs": {"security.capability": "\x01\x00\x00\x02\x00\x20\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"}}),
               ("bin/hl1", F, {"data": b"shared\n"}), ("bin/hl2", "link", {"to": "bin/hl1"}),
               ("dev", D, {}), ("dev/null", "chr", {"dev": (1, 3), "mode": 0o666}),
               ("dev/sda", "blk", {"dev": (8, 0)}), ("dev/fifo", "fifo", {}),
               ("big-ids", F, {"uid": 3000000, "gid": 4000000}),
               ("home", D, {}), ("home/u", D, {"uid": 1000, "gid": 1000, "mode": 0o700}),
               ("home/u/.profile", F, {"uid": 1000, "gid": 1000}),
               (LONG, D, {}), (LONG + "/" + "f" * 120, F, {}),
               ("sym-long", "sym", {"to": "../" * 40 + "target"}),
               ("sym-dir", "sym", {"to": "etc/"}),
               ("names", D, {}), ("names/café", F, {}), ("names/sp ace", F, {}),
               ]),
        # 1: deletions and replacements
        layer([("etc", D, {"mode": 0o750}), ("etc/.wh.a", F, {}), ("etc/sub/.wh.deeper", F, {}),
               ("opq", D, {}), ("opq/.wh..wh..opq", F, {}), ("opq/keep", F, {"data": b"new keep\n"}),
               ("opq/d2", D, {}), ("opq/d2/new", F, {}),
               ("f2d", D, {}), ("f2d/in", F, {}), ("d2f", F, {}),
               ("bin/hl1", F, {"data": b"replaced, both names\n"}), ("bin/hl2", "link", {"to": "bin/hl1"}),
               ("newdir", D, {}), ("newdir/n", F, {}),
               ]),
        # 2: empty
        layer([]),
        # 3: a directory deleted, then made again in the next layer
        layer([(".wh.home", F, {}), ("etc/sub/.wh.x", F, {})]),
        layer([("home", D, {}), ("home/v", F, {}), ("etc/sub/x", D, {}), ("etc/sub/x/now-a-dir", F, {}),
               ("newdir/.wh.n", F, {})]),
        # 5: a whiteout and the same name in one layer, and a whiteout of what never was
        layer([("etc/b", F, {"data": b"b again\n"}), ("etc/.wh.b", F, {}), ("etc/.wh.never", F, {}),
               ("opq/d", D, {}), ("opq/d/.wh..wh..opq", F, {}), ("opq/d/fresh", F, {})]),
    ]


if __name__ == "__main__":
    if sys.argv[1] == "prefix":
        _, _, src, out, k = sys.argv
        write_archive(out, read_archive(src)[: int(k)])
    elif sys.argv[1] == "synth":
        write_archive(sys.argv[2], synth())
