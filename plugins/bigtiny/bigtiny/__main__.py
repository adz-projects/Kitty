from __future__ import annotations

import argparse
import sys


def main() -> None:
    # Kitty spawns this as a child process and relays stdout/stderr into its
    # own log (see util::capture_output) by reading UTF-8 text lines. Without
    # this, stdout/stderr fall back to Windows' ambient locale encoding when
    # not attached to a real console (i.e. always, once piped) — any
    # non-ASCII character in a log line (this codebase's own comments/error
    # text use em-dashes/curly quotes throughout) then encodes as a byte
    # sequence that isn't valid UTF-8, which permanently breaks Kitty's relay
    # thread on the first such line and cascades into "--- Logging error ---"
    # / `OSError: [Errno 22] Invalid argument` spam on every write afterward.
    # `errors="replace"` additionally means a real encoding problem degrades
    # to `?` characters in the log instead of ever raising here.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")

    parser = argparse.ArgumentParser(description="BigTiny daemon")
    parser.add_argument("--host", default="127.0.0.1", help="Bind address")
    parser.add_argument("--port", type=int, default=8080, help="Bind port")
    parser.add_argument("--reload", action="store_true", help="Auto-reload on changes")
    parser.add_argument("--config", help="Path to config YAML file")
    parser.add_argument(
        "--secret",
        help="API secret; clients must send it as X-API-Key "
        "(equivalent to setting BIGTINY_SECRET)",
    )
    args = parser.parse_args()

    import os

    if args.secret:
        # via env so uvicorn reload/worker subprocesses inherit it
        os.environ["BIGTINY_SECRET"] = args.secret

    if args.config:
        # `server.app:create_app` is invoked by uvicorn as a factory string
        # rather than called directly, and its `lifespan()` builds its own
        # config independently of this function's local `config` — passing
        # the path via env is what makes that later load_config() call see
        # the same --config file this process was started with.
        os.environ["BIGTINY_CONFIG_PATH"] = args.config

    from bigtiny.config import load_config
    from bigtiny.logging_config import setup_logging

    config = load_config(args.config)
    setup_logging(level=config.logging.level, json_format=config.logging.json_format)

    import uvicorn

    uvicorn.run(
        "bigtiny.server.app:create_app",
        host=args.host or config.server.host,
        port=args.port or config.server.port,
        reload=args.reload or config.server.reload,
        factory=True,
        loop="bigtiny.server.app:loop_factory",
    )


if __name__ == "__main__":
    main()
