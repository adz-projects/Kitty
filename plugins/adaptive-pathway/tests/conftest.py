import sys
from pathlib import Path

# Unit tests import via `from src.adaptive_pathway...` (treating `src` as a
# namespace package rooted at the repo root), while integration tests import
# the installed package via `from adaptive_pathway...`. The former only
# resolves if the repo root is on sys.path, which previously depended on
# pytest being invoked from the repo root. Insert it explicitly so the suite
# is invocation-directory independent.
_REPO_ROOT = Path(__file__).resolve().parent.parent
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))
