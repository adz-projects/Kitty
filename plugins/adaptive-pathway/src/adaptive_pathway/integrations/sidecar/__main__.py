import argparse
import os

from adaptive_pathway import AdaptivePathway
from adaptive_pathway.integrations.sidecar.server import run_server


def main():
    parser = argparse.ArgumentParser(description="Adaptive Pathway HTTP sidecar")
    parser.add_argument(
        "--db-path",
        default=os.environ.get("ADAPTIVE_PATHWAY_DB", "./pathway.db"),
        help="Path to the SQLite database (default: ./pathway.db)",
    )
    parser.add_argument(
        "--config-path",
        default=None,
        help="Path to a custom defaults.yaml (default: the built-in config)",
    )
    parser.add_argument("--host", default="127.0.0.1", help="Bind host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=8700, help="Bind port (default: 8700)")
    args = parser.parse_args()

    ap = AdaptivePathway(db_path=args.db_path, config_path=args.config_path)
    run_server(ap, host=args.host, port=args.port)


if __name__ == "__main__":
    main()
