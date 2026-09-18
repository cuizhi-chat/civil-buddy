"""Start the Python workbench honoring demo/.env (CIVIL_HOST / CIVIL_PORT).

    python serve.py            # 127.0.0.1:8765
    CIVIL_HOST=0.0.0.0 python serve.py   # phones on the same LAN

`uvicorn app:app --host ... --port ...` still works; this wrapper only reads .env first.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import config  # noqa: E402,F401  (loads .env)


def main() -> None:
    import uvicorn

    host = (os.environ.get("CIVIL_HOST") or "127.0.0.1").strip()
    port = int(os.environ.get("CIVIL_PORT") or 8765)
    if host not in {"127.0.0.1", "localhost", "::1"}:
        print(
            f"warning: binding {host}:{port} — no auth; /api/local can read files on this machine. Trusted LAN only.",
            file=sys.stderr,
        )
    uvicorn.run("app:app", host=host, port=port, reload=bool(os.environ.get("CIVIL_RELOAD")))


if __name__ == "__main__":
    main()
