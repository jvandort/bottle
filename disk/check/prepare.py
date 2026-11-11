"""prepare.py ROOT LAYER: readies ROOT for GNU tar to extract LAYER (an uncompressed tar) onto: the
layer's whiteouts applied, and whatever it replaces with something of another type, a directory
with a file or a file with a directory, removed."""
import os, shutil, sys, tarfile

root, layer = sys.argv[1:]


def remove(p):
    if os.path.isdir(p) and not os.path.islink(p):
        shutil.rmtree(p)
    elif os.path.lexists(p):
        os.unlink(p)


with tarfile.open(layer) as t:
    members = t.getmembers()
# Whiteouts first: they're of the layers below, wherever they are in the tar.
for m in members:
    name = os.path.normpath(m.name)
    d, base = os.path.split(name)
    if base == ".wh..wh..opq":
        if os.path.isdir(os.path.join(root, d)):
            for child in os.listdir(os.path.join(root, d)):
                remove(os.path.join(root, d, child))
    elif base.startswith(".wh."):
        remove(os.path.join(root, d, base[4:]))
for m in members:
    name = os.path.normpath(m.name)
    if os.path.basename(name).startswith(".wh.") or name == ".":
        continue
    p = os.path.join(root, name)
    if not os.path.lexists(p):
        continue
    is_dir = os.path.isdir(p) and not os.path.islink(p)
    if m.isdir() != is_dir:
        remove(p)
