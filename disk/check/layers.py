"""layers.py ARCHIVE DIR: each of ARCHIVE's layers, uncompressed, as DIR/0, DIR/1, ..."""
import gzip, os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mkoci import read_archive

src, out = sys.argv[1:]
os.makedirs(out)
for i, (blob, mt, _) in enumerate(read_archive(src)):
    with open(os.path.join(out, str(i)), "wb") as f:
        f.write(gzip.decompress(blob) if mt.endswith("gzip") else blob)
