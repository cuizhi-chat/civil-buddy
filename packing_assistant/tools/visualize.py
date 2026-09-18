"""
2D 布局出图：基于 container_plan 用 matplotlib 绘制侧视示意。

多柜时：每柜一张图 + 可选总览网格图。
"""

from __future__ import annotations

import os
from datetime import datetime
from typing import Any, Dict, List, Optional, Tuple

# uvicorn 工作线程禁止弹 GUI；必须在 import pyplot 前设 Agg
os.environ.setdefault("MPLBACKEND", "Agg")
try:
    import matplotlib

    matplotlib.use("Agg")
except Exception:
    pass

from packing_assistant.tools.consolidation import CONTAINER_SPECS
from packing_assistant.runtime import cancel as _cancel

_COLOR_CYCLE = [
    "#4C78A8",
    "#54A24B",
    "#F58518",
    "#B279A2",
    "#72B7B2",
    "#9D755D",
    "#E45756",
    "#BAB0AC",
]


def _setup_cjk_font() -> None:
    try:
        import matplotlib.pyplot as plt
        from matplotlib import font_manager
    except ImportError:
        return
    for font_name in ("Microsoft YaHei", "SimHei", "SimSun", "Arial Unicode MS"):
        try:
            plt.rcParams["font.sans-serif"] = [font_name, "DejaVu Sans"]
            plt.rcParams["axes.unicode_minus"] = False
            font_manager.findfont(font_name, fallback_to_default=False)
            break
        except Exception:
            continue


def _draw_one_ax(
    ax,
    items: List[Dict[str, Any]],
    max_len: float,
    title: str,
    *,
    annotations: Optional[List[Dict[str, Any]]] = None,
) -> None:
    from matplotlib.patches import FancyBboxPatch, Rectangle

    ax.add_patch(
        Rectangle(
            (0, 0),
            max_len,
            1.0,
            fill=False,
            edgecolor="black",
            linewidth=2,
        )
    )
    pad_ids = set()
    for a in annotations or []:
        if a.get("type") == "pad_beam" and a.get("box_id"):
            pad_ids.add(str(a.get("box_id")))

    for i, item in enumerate(items):
        _cancel.check()
        start = float(item.get("起始位置_m") or item.get("x_m") or 0)
        length = float(item.get("长度_m") or item.get("dx_m") or 0.5)
        box_id = item.get("箱号") or item.get("box_id") or ""
        face = _COLOR_CYCLE[i % len(_COLOR_CYCLE)]
        is_pad = str(box_id) in pad_ids
        ax.add_patch(
            FancyBboxPatch(
                (start, 0.15),
                length,
                0.7,
                boxstyle="round,pad=0.02,rounding_size=0.05",
                facecolor=face,
                edgecolor="#c0392b" if is_pad else "white",
                linewidth=2.2 if is_pad else 1.2,
                alpha=0.9,
            )
        )
        ax.text(
            start + length / 2,
            0.5,
            box_id,
            ha="center",
            va="center",
            fontsize=8,
            color="white",
            fontweight="bold",
        )
        if is_pad:
            ax.text(
                start + length / 2,
                0.95,
                "垫梁",
                ha="center",
                va="bottom",
                fontsize=7,
                color="#c0392b",
                fontweight="bold",
            )

    # 空隙：在柜底画橙色竖线/区间
    for a in annotations or []:
        if a.get("type") != "void_fill":
            continue
        gap_mm = float(a.get("gap_mm") or 0)
        if gap_mm <= 0:
            continue
        # 优先用 layout 算出的真实 x_m
        if a.get("x_m") is not None:
            x0 = float(a.get("x_m"))
        elif a.get("x_mm") is not None:
            x0 = float(a.get("x_mm")) / 1000.0
        else:
            x0 = max_len * 0.35
        w = min(max_len * 0.25, max(0.12, gap_mm / 1000.0))
        ax.axvspan(x0, min(max_len, x0 + w), ymin=0.05, ymax=0.12, color="#e67e22", alpha=0.85)
        ax.text(
            x0 + w / 2,
            0.02,
            f"空隙{gap_mm:.0f}mm",
            ha="center",
            va="bottom",
            fontsize=6,
            color="#d35400",
        )

    ax.set_xlim(-0.2, max_len + 0.5)
    ax.set_ylim(-0.15, 1.35)
    ax.set_xlabel("柜长方向 (m)")
    ax.set_yticks([])
    ax.set_title(title, fontsize=10)
    ax.set_aspect("equal", adjustable="box")
    ax.grid(axis="x", linestyle="--", alpha=0.4)


def _annotations_for_container(
    plan: Dict[str, Any], cno: int
) -> List[Dict[str, Any]]:
    """从 secure_work_order / layout 提取本柜标注。"""
    swo = plan.get("secure_work_order") or {}
    items = list(swo.get("items") or [])
    if not items:
        for key in ("void_fills", "pad_beams"):
            for row in swo.get(key) or []:
                items.append(row)
    out: List[Dict[str, Any]] = []
    layout = plan.get("layout") or []
    pos_by_id: Dict[str, float] = {}
    for it in layout:
        if int(it.get("container_no") or 1) != cno:
            continue
        bid = str(it.get("box_id") or "")
        pos = it.get("position") or {}
        if bid:
            pos_by_id[bid] = float(pos.get("x") or 0) / 1000.0
    for row in items:
        rc = row.get("container_no")
        if rc is not None and int(rc) != cno:
            continue
        r = dict(row)
        bid = str(r.get("box_id") or "")
        if bid and bid in pos_by_id:
            r["x_m"] = pos_by_id[bid]
        # void 无柜号时各柜都淡标一次
        if r.get("type") == "void_fill" or r.get("type") == "pad_beam" or r.get("type") in (
            "void_fill",
            "pad_beam",
            "strapping",
        ):
            if rc is None and r.get("type") == "void_fill" and cno != 1:
                continue
            out.append(r)
    return out


def draw_layout(
    container_plan: Dict[str, Any],
    output_dir: str = "output",
    filename: Optional[str] = None,
) -> str:
    """
    生成 2D 布局图，返回图片路径。

    若布局项含 柜号/container_no 且多于 1 柜，仍画「全部叠在一柜」的兼容图
    （旧行为）；推荐改用 draw_layout_multi。
    """
    os.makedirs(output_dir, exist_ok=True)
    if not filename:
        ts = datetime.now().strftime("%Y%m%d_%H%M%S")
        filename = f"layout_{ts}.png"
    path = os.path.join(output_dir, filename)

    layout = container_plan.get("布局") or []
    ctype = container_plan.get("柜型") or "40HQ"
    spec = CONTAINER_SPECS.get(ctype, CONTAINER_SPECS["40HQ"])
    max_len = float(spec["长_m"])

    try:
        import matplotlib.pyplot as plt
    except ImportError:
        txt_path = path.replace(".png", ".txt")
        with open(txt_path, "w", encoding="utf-8") as f:
            f.write(f"柜型: {ctype}\n结论: {container_plan.get('结论')}\n布局:\n")
            for item in layout:
                f.write(f"  {item}\n")
        return txt_path

    _setup_cjk_font()
    fig, ax = plt.subplots(figsize=(12, 3.5))
    outer_label = (
        container_plan.get("外廓摆柜率")
        or container_plan.get("空间利用率")
        or "-"
    )
    book_label = container_plan.get("订柜有效体积率") or "-"
    title = (
        f"{ctype} 拼柜布局 | {container_plan.get('结论', '')} | "
        f"外廓摆柜 {outer_label} / 订柜有效体积 {book_label} / "
        f"重量 {container_plan.get('重量利用率', '-')} "
        f"（外廓≠订柜）"
    )
    _draw_one_ax(ax, layout, max_len, title)
    fig.tight_layout()
    fig.savefig(path, dpi=150, bbox_inches="tight")
    plt.close(fig)
    return path


def draw_layout_multi(
    plan: Dict[str, Any],
    *,
    container_type: str = "40HQ",
    output_dir: str = "output",
    prefix: Optional[str] = None,
) -> Dict[str, Any]:
    """
    按 container_no 为每柜出侧视图，并生成总览网格。

    返回:
      {
        "per_container": [{"container_no": 1, "path": "...", "boxes": n}, ...],
        "overview_path": "...",
        "primary_path": 第一柜或总览路径,
      }
    """
    os.makedirs(output_dir, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    prefix = prefix or f"side_{ts}"
    ctype = container_type or plan.get("container_type") or "40HQ"
    spec = CONTAINER_SPECS.get(ctype, CONTAINER_SPECS["40HQ"])
    max_len = float(spec["长_m"])

    layout = plan.get("layout") or plan.get("布局") or []
    # 归一化为分组
    groups: Dict[int, List[Dict[str, Any]]] = {}
    for it in layout:
        cno = int(it.get("container_no") or it.get("柜号") or 1)
        pos = it.get("position") or {}
        size = it.get("size") or {}
        if pos.get("x") is not None or size.get("dx") is not None:
            x_m = float(pos.get("x") or 0) / 1000.0
            dx_m = float(size.get("dx") or 0) / 1000.0
        else:
            x_m = float(it.get("起始位置_m") or 0)
            dx_m = float(it.get("长度_m") or 0.5)
        item = {
            "箱号": it.get("box_id") or it.get("箱号") or "",
            "起始位置_m": x_m,
            "长度_m": dx_m,
        }
        groups.setdefault(cno, []).append(item)

    if not groups:
        return {"per_container": [], "overview_path": None, "primary_path": None}

    try:
        import matplotlib.pyplot as plt
    except ImportError:
        return {"per_container": [], "overview_path": None, "primary_path": None}

    _setup_cjk_font()
    outer_u = float(
        plan.get("outer_space_utilization") or plan.get("space_utilization") or 0
    )
    book_u = float(plan.get("booking_volume_utilization") or 0)
    weight = float(plan.get("weight_utilization") or 0)
    can = plan.get("can_fit")
    msg = plan.get("message") or ("可装下" if can else "未完全装下")
    dual = (
        f"订柜有效体积{book_u*100:.0f}%｜外廓摆柜{outer_u*100:.0f}%｜重量{weight*100:.0f}%"
        f"（外廓≠订柜）"
    )
    per_stats = {
        int(p.get("container_no") or 0): p for p in (plan.get("per_container") or [])
    }

    per_out: List[Dict[str, Any]] = []
    paths: List[str] = []
    for cno in sorted(groups.keys()):
        items = groups[cno]
        st = per_stats.get(cno) or {}
        vol = st.get("volume_utilization")
        load = st.get("load_kg")
        nbox = st.get("boxes") or len(items)
        sub = f"第{cno}柜/{len(groups)} | {nbox}箱"
        if vol is not None:
            # per-container volume_utilization 是外廓几何，禁止写成订柜
            sub += f" | 外廓{float(vol)*100:.0f}%"
        if load is not None:
            sub += f" | {float(load):.0f}kg"
        anns = _annotations_for_container(plan, cno)
        n_ann = len(anns)
        if n_ann:
            sub += f" | 工单标{n_ann}"
        title = f"{ctype} {sub} | {dual}"
        path = os.path.join(output_dir, f"{prefix}_c{cno:02d}.png")
        fig, ax = plt.subplots(figsize=(12, 2.8))
        _draw_one_ax(ax, items, max_len, title, annotations=anns)
        fig.tight_layout()
        fig.savefig(path, dpi=140, bbox_inches="tight")
        plt.close(fig)
        per_out.append(
            {
                "container_no": cno,
                "path": path,
                "boxes": len(items),
                "annotations": n_ann,
            }
        )
        paths.append(path)

    # 总览：最多 16 柜一页网格
    n = len(groups)
    cols = 2 if n <= 4 else (3 if n <= 9 else 4)
    rows = (n + cols - 1) // cols
    fig, axes = plt.subplots(rows, cols, figsize=(6 * cols, 2.6 * rows))
    if rows == 1 and cols == 1:
        axes_list = [axes]
    else:
        axes_list = list(axes.flatten()) if hasattr(axes, "flatten") else [axes]
    for idx, cno in enumerate(sorted(groups.keys())):
        _cancel.check()  # one 3D/side panel per container can take a second each
        ax = axes_list[idx]
        st = per_stats.get(cno) or {}
        nbox = st.get("boxes") or len(groups[cno])
        vol = st.get("volume_utilization")
        t = f"#{cno} · {nbox}箱"
        if vol is not None:
            t += f" · {float(vol)*100:.0f}%"
        _draw_one_ax(ax, groups[cno], max_len, t)
    for j in range(len(groups), len(axes_list)):
        axes_list[j].axis("off")
    fig.suptitle(
        f"{ctype} 共{n}柜侧视总览 | {msg} | {dual}",
        fontsize=12,
        y=1.01,
    )
    fig.tight_layout()
    overview = os.path.join(output_dir, f"{prefix}_overview.png")
    fig.savefig(overview, dpi=130, bbox_inches="tight")
    plt.close(fig)

    return {
        "per_container": per_out,
        "overview_path": overview,
        "primary_path": overview or (paths[0] if paths else None),
        "all_paths": paths + ([overview] if overview else []),
    }
