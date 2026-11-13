# `python3 -m folio`: the interpreter checked, then folio. Keep this file Python 3.9-compatible so
# macOS's /usr/bin/python3 gets a clear error.
import sys

if sys.version_info < (3, 11):
    sys.exit(f"folio requires Python 3.11+ (found {sys.version.split()[0]} at {sys.executable})")

from folio.cli import main  # noqa: E402

sys.exit(main(sys.argv[1:]))
