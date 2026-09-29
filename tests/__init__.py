import atexit
import os
import shutil
import tempfile

# No test touches the real ~/.bottle: tests that need a BOTTLE_HOME of their own set one.
_home = tempfile.mkdtemp(prefix="bottle-tests-")
atexit.register(shutil.rmtree, _home, ignore_errors=True)
os.environ["BOTTLE_HOME"] = _home
