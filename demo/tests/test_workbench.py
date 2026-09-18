"""HTTP + kbio + mention tests against shipped app (no DeepSeek required)."""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from catalog import get_expert, resolve_mentions  # noqa: E402
from kbio import resolve_rel  # noqa: E402


@pytest.fixture()
def client():
    from fastapi.testclient import TestClient

    from app import app

    return TestClient(app)


def test_index_and_static(client):
    r = client.get("/")
    assert r.status_code == 200
    assert "Civil Buddy" in r.text
    js = client.get("/static/app.js")
    assert js.status_code == 200
    assert "reloadCatalog" in js.text
    assert "全企业" in client.get("/").text or "任意专家" in client.get("/").text


def test_catalog_sixteen(client):
    r = client.get("/api/catalog")
    assert r.status_code == 200
    body = r.json()
    assert len(body["categories"]) == 16
    assert len(body["experts"]) == 66
    ids = {e["id"] for e in body["experts"]}
    for need in ("interior", "facade", "civil-defense", "hydraulic", "port"):
        assert need in ids


def test_studio_tree_and_read(client):
    tree = client.get("/api/studio/tree").json()
    assert "company" in tree
    assert len(tree["categories"]) == 16
    got = client.get("/api/studio/file", params={"path": "company/ask-anyone.md"})
    assert got.status_code == 200
    assert "任何人都可以向你提问" in got.json()["content"] or "全企业" in got.json()["content"]


def test_path_traversal_blocked():
    assert resolve_rel("../secrets.md") is None
    assert resolve_rel("company/../../config.py") is None


def test_studio_rejects_traversal(client):
    r = client.get("/api/studio/file", params={"path": "../README.md"})
    assert r.status_code == 404
    w = client.put("/api/studio/file", json={"path": "../x.md", "content": "nope"})
    assert w.status_code == 400


def test_kb_list_has_layers(client):
    r = client.get("/api/kb/structure")
    assert r.status_code == 200
    layers = {f["layer"] for f in r.json()["files"]}
    assert "expert" in layers
    assert "category" in layers
    assert "company" in layers


def test_unknown_expert_404(client):
    assert client.get("/api/kb/not-a-real-expert").status_code == 404


def test_mentions_do_not_summon_every_construction_sentence():
    """A finance question containing 施工 must not auto-summon 施工方案."""
    ids = resolve_mentions("财务上施工发票备注栏怎么写？")
    assert "construction" not in ids


def test_studio_crud_roundtrip(client):
    path = "company/_pytest_roundtrip.md"
    w = client.put("/api/studio/file", json={"path": path, "content": "# tmp\nhello-kb\n"})
    assert w.status_code == 200, w.text
    r = client.get("/api/studio/file", params={"path": path})
    assert r.status_code == 200
    assert "hello-kb" in r.json()["content"]
    d = client.delete("/api/studio/file", params={"path": path})
    assert d.status_code == 200
    assert client.get("/api/studio/file", params={"path": path}).status_code == 404


def test_invalid_expert_id_rejected(client):
    r = client.post(
        "/api/studio/experts",
        json={"id": "Bad ID", "name": "x", "category": "design"},
    )
    assert r.status_code == 400


def test_chat_plain_when_no_explicit_summon(client, monkeypatch):
    monkeypatch.setattr("app.has_key", lambda: True)

    def fake_plain(history, **_kw):
        yield {"event": "done", "data": {"mode": "plain", "text": "PLAIN", "citations": [], "deliverables": []}}

    monkeypatch.setattr("app.run_plain", fake_plain)
    r = client.post("/api/chat", json={"message": "配合比和发票有什么关系", "expert_ids": []})
    assert r.status_code == 200
    assert "PLAIN" in r.text


def test_explicit_mention_still_works():
    ids = resolve_mentions("请 @施工方案 写临边提纲")
    assert "construction" in ids
    ids2 = resolve_mentions("召唤危大识别：临边要不要论证")
    assert "method-hazard" in ids2
    assert "construction" not in ids2
