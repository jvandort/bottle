"""walk.py DIR: a line for everything under DIR: path, type, mode, owner, size, mtime, link
target, content hash, xattrs, device, and which hard link group it's in."""
import hashlib, os, stat, sys

root = sys.argv[1].encode()
groups = {}
lines = []
for dirpath, dirs, files in os.walk(root):
    dirs.sort()
    for name in sorted(dirs + files):
        p = os.path.join(dirpath, name)
        rel = os.path.relpath(p, root)
        st = os.lstat(p)
        kind = stat.S_IFMT(st.st_mode)
        extra = b""
        if stat.S_ISREG(kind):
            with open(p, "rb") as f:
                extra = hashlib.sha256(f.read()).hexdigest()[:12].encode()
            if st.st_nlink > 1:
                g = groups.setdefault((st.st_dev, st.st_ino), len(groups))
                extra += b" hl%d" % g
        elif stat.S_ISLNK(kind):
            extra = b"-> " + os.readlink(p)
        elif stat.S_ISCHR(kind) or stat.S_ISBLK(kind):
            extra = b"dev %d,%d" % (os.major(st.st_rdev), os.minor(st.st_rdev))
        try:
            xs = sorted(os.listxattr(p, follow_symlinks=False))
        except OSError:
            xs = []
        xa = b" ".join(x.encode() + b"=" + os.getxattr(p, x, follow_symlinks=False).hex().encode()
                       for x in xs if not x.startswith("trusted.overlay"))
        mtime = b"" if stat.S_ISDIR(kind) else b"%d" % int(st.st_mtime)
        size = b"%d" % st.st_size if stat.S_ISREG(kind) else b""
        lines.append(b"%s %o %04o %d:%d %s %s %s %s" % (
            rel, kind >> 12, stat.S_IMODE(st.st_mode), st.st_uid, st.st_gid, size, mtime, extra, xa))
sys.stdout.buffer.write(b"\n".join(lines) + b"\n")
