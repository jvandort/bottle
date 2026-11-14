"""folio's tests. folio's Python runs folio-cli (features, runners), so the tests run this checkout's, built once
(debug), unless $FOLIO_CLI names one already."""

import os
import shutil
import subprocess
from pathlib import Path

if not os.environ.get("FOLIO_CLI"):
    _repo = Path(__file__).resolve().parents[2]
    _cargo = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / "cargo")
    subprocess.run([_cargo, "build", "-q", "-p", "folio-cli"], cwd=_repo, check=True)
    os.environ["FOLIO_CLI"] = str(Path(os.environ.get("CARGO_TARGET_DIR") or _repo / "target") / "debug" / "folio-cli")
