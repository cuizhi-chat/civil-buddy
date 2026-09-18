"""Cooperative cancel registry for long runs that are not Scheduler runs (packing pipeline, exports).

Keyed by run_id *or* session_id: the packing loop checks both at every agent boundary
(teams/big_team.run_one) and skips the remaining agents with status "cancelled".
Entries expire so a stale request can never block a later run of the same session.
"""

from __future__ import annotations

import contextvars
import time
from threading import Lock
from typing import Dict, Iterator, Tuple

# keys (run_id, session_id) of the run executing on this thread — set by big_team.run_one
_CURRENT: contextvars.ContextVar[Tuple[str, ...]] = contextvars.ContextVar("civil_cancel_keys", default=())


class RunCancelled(Exception):
    """Raised by check() inside a long tool loop when the user asked to stop."""

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


class scope:
    """`with cancel.scope(run_id, session_id):` — makes check() aware of the current run."""

    def __init__(self, *keys: str) -> None:
        self._keys = tuple(k for k in keys if k)
        self._token = None

    def __enter__(self) -> "scope":
        self._token = _CURRENT.set(self._keys)
        return self

    def __exit__(self, *exc: object) -> None:
        if self._token is not None:
            _CURRENT.reset(self._token)


def current_keys() -> Tuple[str, ...]:
    return _CURRENT.get()


def check() -> None:
    """Cheap cooperative checkpoint for hot loops (bin3d placement, LLM rounds). No-op outside a scope."""
    if not _REQUESTED:  # fast path: nothing was ever cancelled → no lock, no contextvar lookup cost
        return
    for k in _CURRENT.get():
        if is_cancelled(k):
            raise RunCancelled(f"cancelled by user ({k})")


def every(n: int) -> Iterator[None]:
    """Yield forever, calling check() every n iterations: `for _ in zip(items, cancel.every(50)):`."""
    i = 0
    while True:
        i += 1
        if i % max(1, n) == 0:
            check()
        yield None
