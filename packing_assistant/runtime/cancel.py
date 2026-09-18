"""Cooperative cancel registry for long runs that are not Scheduler runs (packing pipeline, exports).

Keyed by run_id *or* session_id: the packing loop checks both at every agent boundary
(teams/big_team.run_one) and skips the remaining agents with status "cancelled".
Entries expire so a stale request can never block a later run of the same session.
"""

from __future__ import annotations

import time
from threading import Lock
from typing import Dict

_TTL_SEC = 3600.0
_LOCK = Lock()
_REQUESTED: Dict[str, float] = {}


def request(key: str) -> bool:
    key = (key or "").strip()
    if not key:
        return False
    with _LOCK:
        _REQUESTED[key] = time.time()
    return True


def is_cancelled(key: str) -> bool:
    key = (key or "").strip()
    if not key:
        return False
    with _LOCK:
        ts = _REQUESTED.get(key)
        if ts is None:
            return False
        if time.time() - ts > _TTL_SEC:
            _REQUESTED.pop(key, None)
            return False
        return True


def clear(*keys: str) -> None:
    with _LOCK:
        for k in keys:
            _REQUESTED.pop((k or "").strip(), None)
