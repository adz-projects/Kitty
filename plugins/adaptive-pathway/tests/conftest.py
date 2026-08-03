import sys
import os
import tempfile
import threading
import time
from pathlib import Path

import pytest
import uvicorn

# Unit tests import via `from src.adaptive_pathway...` (rooted at the repo
# root), while integration tests import the installed package via
# `from adaptive_pathway...`. The former only resolves if the repo root is on
# sys.path, which previously depended on pytest being invoked from the repo
# root. Insert it explicitly so the suite is invocation-directory independent.
# (`src/__init__.py` exists so this is a regular package — a namespace portion
# would lose to any unrelated regular package named `src` elsewhere on
# sys.path, e.g. another project's editable install.)
_REPO_ROOT = Path(__file__).resolve().parent.parent
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))


@pytest.fixture(scope="session")
def proxy_env():
    """A live sidecar (uvicorn on an ephemeral port) plus the env dict every
    MCP-stdio subprocess needs to reach it.

    The MCP server is a stateless HTTP proxy to the sidecar (the sidecar
    owns the single engine + DB), so every subprocess-based MCP test needs
    a real sidecar behind it."""
    from adaptive_pathway import AdaptivePathway
    from adaptive_pathway.integrations.sidecar.server import create_app

    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)

    ap = AdaptivePathway(db_path=path)
    app = create_app(ap)
    config = uvicorn.Config(app, host="127.0.0.1", port=0, log_level="warning")
    server = uvicorn.Server(config)
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    for _ in range(100):
        if server.started:
            break
        time.sleep(0.05)
    port = server.servers[0].sockets[0].getsockname()[1]

    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = path
    env["AP_SIDECAR_PORT"] = str(port)

    yield env

    server.should_exit = True
    thread.join(timeout=10)
    try:
        os.remove(path)
    except PermissionError:
        pass
