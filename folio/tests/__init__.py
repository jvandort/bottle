"""folio's tests: they run folio, this checkout's, built once (debug) as the suite starts, unless
$FOLIO_UNDER_TEST names another (a release build, say)."""

import os
import shutil
import subprocess
from pathlib import Path

if not os.environ.get("FOLIO_UNDER_TEST"):
    _repo = Path(__file__).resolve().parents[2]
    _cargo = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / "cargo")
    subprocess.run([_cargo, "build", "-q", "-p", "folio"], cwd=_repo, check=True)
    os.environ["FOLIO_UNDER_TEST"] = str(Path(os.environ.get("CARGO_TARGET_DIR") or _repo / "target") / "debug" / "folio")
