"""Session attachments for the Python workbench.

Mirrors workbench/src/attach.rs: same directory layout (data/uploads/<session>/<id>.{bin,txt,json}),
same limits, same prompt bundle shape — so an upload made through either backend is visible to both.
"""

from __future__ import annotations

import io
import json
import re
import uuid
import zipfile
from pathlib import Path
from typing import Any
from xml.etree import ElementTree as ET

from config import DEMO_ROOT

UPLOAD_ROOT = DEMO_ROOT / "data" / "uploads"
MAX_BYTES = 20 * 1024 * 1024
MAX_TEXT_CHARS = 200_000
MAX_FILES = 12
INJECT_CHARS = 60_000
ALLOWED_EXT = ("pdf", "docx", "xlsx", "txt", "md", "csv", "json", "log")
_SESSION_RE = re.compile(r"[^A-Za-z0-9_-]")
_NAME_KEEP = re.compile(r"[^A-Za-z0-9._\-（）\u4e00-\u9fff]")


def sanitize_session(session: str) -> str:
    s = _SESSION_RE.sub("", session or "")[:32]
    if len(s) < 4:
        raise ValueError("session_id 无效")
    return s


def session_dir(session: str) -> Path:
    d = UPLOAD_ROOT / sanitize_session(session)
    d.mkdir(parents=True, exist_ok=True)
    return d


def safe_filename(name: str) -> str:
    raw = Path(name or "").name or "upload.bin"
    cleaned = _NAME_KEEP.sub("_", raw)
    return (cleaned or "upload.bin")[:80]


def _ext(name: str) -> str:
    return Path(name).suffix.lower().lstrip(".")


def _decode(data: bytes) -> str:
    for enc in ("utf-8-sig", "utf-8", "gb18030"):
        try:
            return data.decode(enc)
        except UnicodeDecodeError:
            continue
    return data.decode("utf-8", errors="ignore")


def _extract_docx(data: bytes) -> str:
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        try:
            xml = z.read("word/document.xml")
        except KeyError as exc:
            raise ValueError("docx 缺少 word/document.xml") from exc
    root = ET.fromstring(xml)
    ns = {"w": "http://schemas.openxmlformats.org/wordprocessingml/2006/main"}
    paras = []
    for p in root.iter(f"{{{ns['w']}}}p"):
        t = "".join(x.text or "" for x in p.iter(f"{{{ns['w']}}}t")).strip()
        if t:
            paras.append(t)
    if not paras:
        raise ValueError("docx 抽不出文字")
    return "\n".join(paras)


def _extract_xlsx(data: bytes) -> str:
    try:
        import openpyxl  # noqa: WPS433
    except ImportError as exc:  # pragma: no cover - environment dependent
        raise ValueError("读 xlsx 需要 openpyxl：pip install openpyxl") from exc
    wb = openpyxl.load_workbook(io.BytesIO(data), read_only=True, data_only=True)
    lines: list[str] = []
    try:
        for ws in wb.worksheets:
            lines.append(f"# {ws.title}")
            for row in ws.iter_rows(max_row=2000, max_col=32, values_only=True):
                cells = ["" if c is None else str(c).strip() for c in row]
                if any(cells):
                    lines.append(" | ".join(cells))
            if sum(len(x) for x in lines) >= MAX_TEXT_CHARS:
                break
    finally:
        wb.close()
    text = "\n".join(lines)
    if not text.strip() or all(line.startswith("# ") for line in lines):
        raise ValueError("xlsx 抽不出可用文字。请另存为 CSV 或把单元格改成文本。")
    return text


def _extract_pdf(data: bytes) -> str:
    try:
        from pypdf import PdfReader  # noqa: WPS433
    except ImportError as exc:  # pragma: no cover - environment dependent
        raise ValueError("读 PDF 需要 pypdf：pip install pypdf（或先转成 docx/txt 再传）") from exc
    reader = PdfReader(io.BytesIO(data))
    parts = []
    for i, page in enumerate(reader.pages):
        try:
            t = page.extract_text() or ""
        except Exception:  # noqa: BLE001 - one bad page must not kill the file
            t = ""
        if t.strip():
            parts.append(f"[第{i + 1}页]\n{t.strip()}")
        if sum(len(x) for x in parts) >= MAX_TEXT_CHARS:
            break
    if not parts:
        raise ValueError("PDF 没有可抽的文字层（可能是扫描件），请先 OCR 或转成 docx/txt。")
    return "\n\n".join(parts)


def extract_text(filename: str, data: bytes) -> tuple[str, str, str]:
    """Return (kind, text, engine). Raises ValueError with a user-facing message."""
    if len(data) > MAX_BYTES:
        raise ValueError(f"单文件不能超过 {MAX_BYTES // 1024 // 1024} MB")
    ext = _ext(filename)
    if ext not in ALLOWED_EXT:
        raise ValueError("只接受 pdf / docx / xlsx / txt / md / csv / json / log")
    if ext == "pdf":
        text, engine = _extract_pdf(data), "pypdf"
    elif ext == "docx":
        text, engine = _extract_docx(data), "zip-xml"
    elif ext == "xlsx":
        text, engine = _extract_xlsx(data), "openpyxl"
    else:
        text, engine = _decode(data), "text"
    text = text[:MAX_TEXT_CHARS]
    if not text.strip():
        raise ValueError("文件里没有可用文字")
    return ext, text, engine


def save_upload(session: str, filename: str, data: bytes) -> dict[str, Any]:
    d = session_dir(session)
    if len(list_uploads(session)) >= MAX_FILES:
        raise ValueError(f"同一会话最多 {MAX_FILES} 个附件")
    kind, text, engine = extract_text(filename, data)
    fid = uuid.uuid4().hex[:12]
    name = safe_filename(filename)
    (d / f"{fid}.bin").write_bytes(data)
    (d / f"{fid}.txt").write_text(text, encoding="utf-8")
    meta = {"id": fid, "name": name, "kind": kind, "bytes": len(data), "chars": len(text), "parse": engine, "layer": "upload"}
    (d / f"{fid}.json").write_text(json.dumps(meta, ensure_ascii=False), encoding="utf-8")
    return meta


def list_uploads(session: str) -> list[dict[str, Any]]:
    try:
        d = session_dir(session)
    except ValueError:
        return []
    out = []
    for p in d.glob("*.json"):
        try:
            v = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(v, dict) and v.get("id"):
            v.setdefault("layer", "upload")
            out.append(v)
    out.sort(key=lambda v: str(v.get("name") or ""))
    return out


def read_upload(session: str, fid: str, offset: int = 0, limit: int = 8000) -> str:
    sid = re.sub(r"[^A-Za-z0-9]", "", fid or "")[:16]
    if not sid:
        raise ValueError("附件 id 无效")
    d = session_dir(session)
    txt = d / f"{sid}.txt"
    if not txt.is_file():
        raise ValueError("附件不存在")
    text = txt.read_text(encoding="utf-8")
    take = min(limit or 8000, 20_000)
    piece = text[offset : offset + take]
    more = max(0, len(text) - offset - len(piece))
    try:
        name = json.loads((d / f"{sid}.json").read_text(encoding="utf-8")).get("name") or sid
    except (OSError, json.JSONDecodeError):
        name = sid
    return f"【用户上传：{name}】offset={offset} 本段{len(piece)}字 剩余约{more}字\n\n{piece}"


def bundle_for_prompt(session: str, ids: list[str], user_msg: str) -> str:
    if not ids:
        return user_msg
    parts = []
    used = 0
    for fid in ids:
        try:
            t = read_upload(session, fid, 0, 20_000)
        except ValueError as exc:
            parts.append(f"附件 {fid}：{exc}")
            continue
        room = max(0, INJECT_CHARS - used)
        if room < 80:
            parts.append(f"（还有附件 {fid} 未贴全文，请用 read_attachment）")
            continue
        cut = t[:room]
        used += len(cut)
        parts.append(cut)
    return "\n\n".join(parts) + "\n\n---\n用户说：\n" + user_msg


def import_local(session: str, raw_path: str) -> list[dict[str, Any]]:
    from packing_assistant.office_job import JOB_EXTS, is_forbidden_layout, job_root, job_root_granted

    raw = (raw_path or "").strip()
    if not raw:
        if not job_root_granted():
            raise ValueError("没有路径，也没有授权作业根（CIVIL_JOB_ROOT）")
        raw = str(job_root())
    p = Path(raw).expanduser()
    if is_forbidden_layout(p):
        raise ValueError("禁止读取 D:\\layout")
    if not p.exists():
        raise ValueError("路径不存在（注意：这是工作台所在电脑的路径，不是手机上的）")
    candidates = [p] if p.is_file() else sorted(x for x in p.iterdir() if x.is_file())
    candidates = [x for x in candidates if x.suffix.lower() in JOB_EXTS or x.suffix.lower() == ".pdf"][:8]
    if not candidates:
        raise ValueError("这个文件夹里没有可抽文字的 pdf/docx/xlsx/txt/md/csv")
    out = []
    errors = []
    for f in candidates:
        try:
            out.append(save_upload(session, f.name, f.read_bytes()))
        except ValueError as exc:
            errors.append(f"{f.name}：{exc}")
    if not out:
        raise ValueError("；".join(errors) or "没有可导入的文件")
    return out
