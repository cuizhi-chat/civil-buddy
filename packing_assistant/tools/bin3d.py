"""
纯 Python 3D 装箱引擎（无需 Java / skjolber）。

策略：Largest Area Fit First + Extreme Point
P0：可叠则优先叠高（stackable + 限高/限层）
P1：支撑面积比 + 箱间绑扎间隙 clearance
P2：可选双策略试装取优（轻量 multi-start，非完整 GRASP）

说明：不是 skjolber 源码，但 I/O 对齐，便于本机无 JDK 时完整联调。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional, Sequence, Tuple

from packing_assistant.runtime import cancel as _cancel


@dataclass
class PackPolicy:
    """柜内装载策略（可由 packing_options 注入）。"""

    # P1：箱间水平间隙（绑扎余量），垂直堆叠允许贴顶面
    clearance_mm: int = 30
    # P1：二层支撑面积比（投影重叠 / 底面积）
    support_ratio_min: float = 0.55
    # P0：堆叠总高度上限（mm）；None=柜内高
    max_stack_height_mm: Optional[int] = None
    # P0：最大层数（含底层）
    max_stack_layers: int = 3
    # P0：可叠箱是否优先抬高
    prefer_stack: bool = True
    # 默认 stackable 判定高度
    stackable_max_box_h_mm: int = 1300
    # prefer_bottom 重量阈值（过高则整票无法叠）
    prefer_bottom_weight_kg: float = 2000.0
    # P2：是否双策略试装
    multi_start: bool = True
    # CTU 软偏好：纵向中段带 + 降低横向力矩（堆叠仍主导）
    cog_aware: bool = True
    # 重心再平衡：加大 mid50 权重，重货优先进中段
    cog_rebalance: bool = False
    # 出运严模式：支撑/承重更严（由 risk 侧配合硬门禁）
    export_strict: bool = False
    # 四点/底心须落在支撑投影内
    corner_support: bool = True
    # 默认下层承重倍数（相对自身毛重）；单箱 max_top_load_kg 优先
    default_top_load_factor: float = 1.5


def _as_bool(v: Any, default: bool = False) -> bool:
    if v is None:
        return default
    if isinstance(v, str):
        return v.lower() not in ("0", "false", "no", "off")
    return bool(v)


def policy_from_options(opts: Optional[Dict[str, Any]] = None) -> PackPolicy:
    o = dict(opts or {})
    gap = o.get("clearance_mm", o.get("lashing_gap_mm", o.get("gap_mm", 30)))
    try:
        gap = int(gap)
    except Exception:
        gap = 30
    gap = max(0, min(80, gap))  # 0–80mm；行业常用 20–50
    sr = o.get("support_ratio_min", o.get("support_ratio", 0.55))
    try:
        sr = float(sr)
    except Exception:
        sr = 0.55
    # 出运严模式默认抬高支撑比
    export_strict = _as_bool(o.get("export_strict"), False)
    if export_strict and o.get("support_ratio_min") is None and o.get("support_ratio") is None:
        sr = 0.70
    sr = max(0.3, min(0.95, sr))
    msh = o.get("max_stack_height_mm")
    try:
        msh = int(msh) if msh is not None else None
    except Exception:
        msh = None
    try:
        layers = int(o.get("max_stack_layers", 3))
    except Exception:
        layers = 3
    prefer_stack = o.get("prefer_stack", o.get("prefer_stack_height", True))
    if isinstance(prefer_stack, str):
        prefer_stack = prefer_stack.lower() not in ("0", "false", "no")
    cog_aware = _as_bool(o.get("cog_aware"), True)
    cog_rebalance = _as_bool(o.get("cog_rebalance"), False)
    if cog_rebalance:
        cog_aware = True
    corner = _as_bool(o.get("corner_support"), True)
    try:
        tlf = float(o.get("default_top_load_factor", 1.5) or 1.5)
    except Exception:
        tlf = 1.5
    return PackPolicy(
        clearance_mm=gap,
        support_ratio_min=sr,
        max_stack_height_mm=msh,
        max_stack_layers=max(1, min(6, layers)),
        prefer_stack=bool(prefer_stack),
        stackable_max_box_h_mm=int(o.get("stackable_max_box_h_mm", 1300) or 1300),
        prefer_bottom_weight_kg=float(o.get("prefer_bottom_weight_kg", 2000) or 2000),
        multi_start=bool(o.get("multi_start", True)),
        cog_aware=bool(cog_aware),
        cog_rebalance=bool(cog_rebalance),
        export_strict=export_strict,
        corner_support=corner,
        default_top_load_factor=max(0.5, min(5.0, tlf)),
    )


# 默认策略；try_place 读取
_ACTIVE_POLICY = PackPolicy()


def set_active_policy(p: PackPolicy) -> None:
    global _ACTIVE_POLICY
    _ACTIVE_POLICY = p


def get_active_policy() -> PackPolicy:
    return _ACTIVE_POLICY


def _container_inner() -> Dict[str, Dict[str, float]]:
    try:
        from packing_assistant.knowledge import container_inner_mm

        data = container_inner_mm()
        if data:
            return data
    except Exception:
        pass
    return {
        "20GP": {"L": 5898, "W": 2352, "H": 2385, "max_load_kg": 21000},
        "40GP": {"L": 12032, "W": 2352, "H": 2385, "max_load_kg": 26680},
        "40HQ": {"L": 12032, "W": 2352, "H": 2698, "max_load_kg": 28610},  # COSCO 铭牌 PAYLOAD
        "45HQ": {"L": 13556, "W": 2352, "H": 2698, "max_load_kg": 27700},
    }


CONTAINER_INNER: Dict[str, Dict[str, float]] = _container_inner()


@dataclass
class Item3D:
    box_id: str
    dx: int
    dy: int
    dz: int
    weight_kg: float
    allow_rotate: bool = True
    no_tip: bool = False  # 铁架等：禁止竖放（高度维不可转到水平外）
    stackable: bool = True  # True: 可上上层 + 顶面可承重；False: 不上上层且顶面不承重
    prefer_bottom: bool = False  # 必须底层
    max_stack_layers: Optional[int] = None  # 本箱允许的最大层号（含自身）
    max_top_load_kg: Optional[float] = None  # 顶面允许叠压总重；None=策略倍数×自身重


@dataclass
class Placement3D:
    box_id: str
    x: int
    y: int
    z: int
    dx: int
    dy: int
    dz: int
    container_no: int = 1
    layer: int = 1
    stackable: bool = True  # 顶面是否允许再叠货
    max_stack_layers: Optional[int] = None
    max_top_load_kg: Optional[float] = None


@dataclass
class Bin3D:
    L: int
    W: int
    H: int
    max_load_kg: float
    container_no: int = 1
    placements: List[Placement3D] = field(default_factory=list)
    # extreme points (x,y,z)
    points: List[Tuple[int, int, int]] = field(default_factory=lambda: [(0, 0, 0)])

    @property
    def used_weight(self) -> float:
        # weight stored externally via map; sum from optional attr
        return float(getattr(self, "_used_weight", 0.0))

    def add_weight(self, w: float) -> None:
        self._used_weight = float(getattr(self, "_used_weight", 0.0)) + w


def _orientations(item: Item3D) -> List[Tuple[int, int, int]]:
    d = (item.dx, item.dy, item.dz)
    if not item.allow_rotate:
        return [d]
    seen = set()
    out = []
    if item.no_tip:
        # 仅水平面旋转：高度维保持为 z
        cands = (
            (d[0], d[1], d[2]),
            (d[1], d[0], d[2]),
        )
    else:
        cands = (
            (d[0], d[1], d[2]),
            (d[0], d[2], d[1]),
            (d[1], d[0], d[2]),
            (d[1], d[2], d[0]),
            (d[2], d[0], d[1]),
            (d[2], d[1], d[0]),
        )
    for dims in cands:
        if dims not in seen:
            seen.add(dims)
            out.append(dims)
    return out


def _overlaps(
    a: Placement3D,
    x: int,
    y: int,
    z: int,
    dx: int,
    dy: int,
    dz: int,
    *,
    gap: int = 0,
) -> bool:
    """
    AABB 碰撞。gap>0 时在水平方向加绑扎余量；
    纯上下堆叠（高度区间相接/分离）不加水平 gap，允许贴顶叠高。
    """
    g = max(0, int(gap or 0))
    # 高度完全分离或刚好相接 → 视为堆叠/不同层，不强制水平间隙
    vert_apart = (z + dz <= a.z) or (a.z + a.dz <= z)
    if vert_apart:
        return not (
            x + dx <= a.x
            or a.x + a.dx <= x
            or y + dy <= a.y
            or a.y + a.dy <= y
            or z + dz <= a.z
            or a.z + a.dz <= z
        )
    # 同层/高度重叠：水平方向保留 clearance
    return not (
        x + dx + g <= a.x
        or a.x + a.dx + g <= x
        or y + dy + g <= a.y
        or a.y + a.dy + g <= y
        or z + dz <= a.z
        or a.z + a.dz <= z
    )


def _fits(
    bin: Bin3D,
    x: int,
    y: int,
    z: int,
    dx: int,
    dy: int,
    dz: int,
    *,
    gap: Optional[int] = None,
) -> bool:
    if x < 0 or y < 0 or z < 0:
        return False
    if x + dx > bin.L or y + dy > bin.W or z + dz > bin.H:
        return False
    g = get_active_policy().clearance_mm if gap is None else int(gap)
    for p in bin.placements:
        if _overlaps(p, x, y, z, dx, dy, dz, gap=g):
            return False
    return True


def _support_hits(
    bin: Bin3D, x: int, y: int, z: int, dx: int, dy: int
) -> Tuple[int, List["Placement3D"]]:
    """采样脚印下方支撑：返回命中数与支撑箱列表（去重）。"""
    if z == 0:
        return 999, []
    samples = [
        (x + dx // 2, y + dy // 2),
        (x + max(dx // 4, 1), y + max(dy // 4, 1)),
        (x + dx - max(dx // 4, 1), y + max(dy // 4, 1)),
        (x + max(dx // 4, 1), y + dy - max(dy // 4, 1)),
        (x + dx - max(dx // 4, 1), y + dy - max(dy // 4, 1)),
        (x + 1, y + 1),
        (x + dx - 1, y + dy - 1),
    ]
    hits = 0
    supports: List[Placement3D] = []
    seen = set()
    for sx, sy in samples:
        for p in bin.placements:
            if p.z + p.dz == z and p.x <= sx < p.x + p.dx and p.y <= sy < p.y + p.dy:
                hits += 1
                if id(p) not in seen:
                    seen.add(id(p))
                    supports.append(p)
                break
    return hits, supports


def _support_area_ratio(
    bin: Bin3D, x: int, y: int, z: int, dx: int, dy: int
) -> float:
    """投影重叠面积 / 本箱底面积；底层为 1.0。"""
    if z == 0:
        return 1.0
    base = float(max(dx * dy, 1))
    area = 0.0
    for p in bin.placements:
        if int(p.z + p.dz) != int(z):
            continue
        ix0 = max(x, p.x)
        iy0 = max(y, p.y)
        ix1 = min(x + dx, p.x + p.dx)
        iy1 = min(y + dy, p.y + p.dy)
        if ix1 > ix0 and iy1 > iy0:
            area += float(ix1 - ix0) * float(iy1 - iy0)
    return min(area / base, 1.0)


def _layer_index(bin: Bin3D, z: int) -> int:
    """估计放置高度 z 对应的层号（1=底层）。"""
    if z <= 0:
        return 1
    max_under = 0
    for p in bin.placements:
        if p.z + p.dz <= z:
            max_under = max(max_under, int(getattr(p, "layer", 1) or 1))
    return max_under + 1


def _support_covers_point(
    bin: Bin3D, z: int, sx: float, sy: float
) -> bool:
    for p in bin.placements:
        if int(p.z + p.dz) != int(z):
            continue
        if p.x <= sx <= p.x + p.dx and p.y <= sy <= p.y + p.dy:
            return True
    return False


def _supported(bin: Bin3D, x: int, y: int, z: int, dx: int, dy: int) -> bool:
    """底部支撑：z==0 或面积比 +（可选）四角/底心落在支撑投影内。"""
    if z == 0:
        return True
    pol = get_active_policy()
    ratio = _support_area_ratio(bin, x, y, z, dx, dy)
    min_r = pol.support_ratio_min
    if ratio + 1e-9 < min_r * (0.85 if not pol.export_strict else 1.0):
        # 严模式必须达标；普通模式允许极小裕度后点采样兜底
        if pol.export_strict or ratio + 1e-9 < max(0.35, min_r * 0.65):
            return False
        hits, _ = _support_hits(bin, x, y, z, dx, dy)
        if hits < 3:
            return False
    # 底心须落在支撑上（CoM-in-support 简化）
    cx, cy = x + dx / 2.0, y + dy / 2.0
    if not _support_covers_point(bin, z, cx, cy):
        return False
    # 四角支撑（corner_support / export_strict）
    if pol.corner_support or pol.export_strict:
        margin = max(1.0, min(dx, dy) * 0.05)
        corners = (
            (x + margin, y + margin),
            (x + dx - margin, y + margin),
            (x + margin, y + dy - margin),
            (x + dx - margin, y + dy - margin),
        )
        ok_c = sum(1 for sx, sy in corners if _support_covers_point(bin, z, sx, sy))
        need = 4 if pol.export_strict else 3
        if ok_c < need:
            return False
    return True


def _stack_ok(
    bin: Bin3D,
    item: Item3D,
    x: int,
    y: int,
    z: int,
    dx: int,
    dy: int,
    dz: int,
    *,
    max_stack_height_mm: int,
    max_stack_layers: int = 6,
) -> Tuple[bool, int, float]:
    """
    堆码合法性（P0）：
    - stackable=False / prefer_bottom → 只能 z==0
    - 上层：下方支撑箱必须 stackable（顶面可承重）
    - 叠高 ≤ max_stack_height_mm 且 ≤ 柜高
    - 层号 ≤ policy/item/support 的 max_stack_layers
    返回 (ok, layer, support_weight_avg)；layer 从 1 起。
    """
    top = z + dz
    if top > bin.H or top > max_stack_height_mm:
        return False, 1, 0.0
    if z == 0:
        return True, 1, 0.0

    # 非底层：自身必须可上上层
    if item.prefer_bottom or not item.stackable:
        return False, 1, 0.0

    # P1：支撑面积比为主；点采样收集支撑箱
    pol = get_active_policy()
    ratio = _support_area_ratio(bin, x, y, z, dx, dy)
    hits, supports = _support_hits(bin, x, y, z, dx, dy)
    if not supports:
        # 从投影重叠补支撑箱列表
        for p in bin.placements:
            if int(p.z + p.dz) != int(z):
                continue
            if not (
                x + dx <= p.x
                or p.x + p.dx <= x
                or y + dy <= p.y
                or p.y + p.dy <= y
            ):
                supports.append(p)
    if not supports:
        return False, 1, 0.0
    if ratio + 1e-9 < pol.support_ratio_min and hits < 2:
        return False, 1, 0.0
    if ratio + 1e-9 < max(0.35, pol.support_ratio_min * 0.65):
        return False, 1, 0.0

    # 支撑箱顶面必须允许承重（stackable=false → 不可压货）
    for s in supports:
        if not bool(getattr(s, "stackable", True)):
            return False, 1, 0.0

    # 底心/四角（与 _supported 一致，export 更严）
    if not _supported(bin, x, y, z, dx, dy):
        return False, 1, 0.0

    base_layer = max(int(getattr(s, "layer", 1) or 1) for s in supports)
    layer = base_layer + 1

    # 全局层数上限 + 单箱/支撑箱层数上限
    if layer > int(max_stack_layers):
        return False, layer, 0.0
    if item.max_stack_layers is not None and layer > int(item.max_stack_layers):
        return False, layer, 0.0
    for s in supports:
        msl = getattr(s, "max_stack_layers", None)
        if msl is not None and layer > int(msl):
            return False, layer, 0.0

    # 下层承重：本箱重量分摊到各支撑箱，不得超过 max_top_load
    item_w = float(item.weight_kg or 0)
    n_sup = max(len(supports), 1)
    share = item_w / n_sup
    for s in supports:
        cap = getattr(s, "max_top_load_kg", None)
        if cap is None:
            sw0 = float(getattr(s, "weight_kg", 0) or 0)
            cap = sw0 * float(pol.default_top_load_factor)
            if cap <= 0:
                cap = item_w * 2  # 无重量信息时放宽
        # 已压在该支撑上的重量（同顶面的其它上层）
        already = 0.0
        top_z = s.z + s.dz
        for p in bin.placements:
            if p.z != top_z:
                continue
            # 投影与支撑有重叠则计部分荷载
            if not (
                p.x + p.dx <= s.x
                or s.x + s.dx <= p.x
                or p.y + p.dy <= s.y
                or s.y + s.dy <= p.y
            ):
                already += float(getattr(p, "weight_kg", 0) or 0)
        if already + share > float(cap) + 1e-6:
            return False, layer, 0.0

    sw = [float(getattr(s, "weight_kg", 0) or 0) for s in supports]
    avg_w = sum(sw) / max(len(sw), 1)
    return True, layer, avg_w


def _add_points(bin: Bin3D, x: int, y: int, z: int, dx: int, dy: int, dz: int) -> None:
    # 更完整的 Extreme Point 集合，利于堆叠与填缝
    candidates = [
        (x + dx, y, z),
        (x, y + dy, z),
        (x, y, z + dz),
        (x + dx, y + dy, z),
        (x + dx, y, z + dz),
        (x, y + dy, z + dz),
    ]
    for c in candidates:
        if 0 <= c[0] <= bin.L and 0 <= c[1] <= bin.W and 0 <= c[2] <= bin.H:
            if c not in bin.points:
                bin.points.append(c)
    # 清理被占满的点；候选过多时截断（保序）
    cleaned = []
    for pt in bin.points:
        inside = any(
            p.x <= pt[0] < p.x + p.dx
            and p.y <= pt[1] < p.y + p.dy
            and p.z <= pt[2] < p.z + p.dz
            for p in bin.placements
        )
        if not inside:
            cleaned.append(pt)
    # 按 z,y,x 排序，优先底层
    cleaned.sort(key=lambda t: (t[2], t[1], t[0]))
    bin.points = cleaned[:400] if len(cleaned) > 400 else cleaned
    if not bin.points:
        bin.points = [(0, 0, 0)]


def try_place(bin: Bin3D, item: Item3D) -> Optional[Placement3D]:
    """
    Extreme-point 放置（P0 堆码 + CTU CoG 软偏好）。

    - stackable 且高度允许时优先叠上层（非纯地面 LAFF）
    - stackable=False：不上上层，且顶面不承重
    - 叠高：packing_options.max_stack_height_mm 或柜内高；层数尊重 policy/单箱 max_stack_layers
    - 重下轻上：软偏好（罚分，非硬拒绝）
    - cog_aware：纵中 60/50（系统质量加权）+ 横向力矩；不再强行贴门端
    """
    if bin.used_weight + item.weight_kg > bin.max_load_kg + 1e-6:
        return None

    pol = get_active_policy()
    gap = max(0, int(pol.clearance_mm or 0))
    max_zh = int(pol.max_stack_height_mm or bin.H)
    max_zh = max(1, min(max_zh, bin.H))
    global_layers = int(pol.max_stack_layers or 6)
    want_stack = bool(pol.prefer_stack and item.stackable and not item.prefer_bottom)
    cog_on = bool(pol.cog_aware)

    # 候选点：extreme points + 地面双列条带 + 中段锚点 + 已放箱邻接 + 顶面堆叠
    pts = list(bin.points)
    pts.append((0, 0, 0))
    for dx0, dy0, _ in _orientations(item):
        pts.append((0, 0, 0))
        if dy0 <= bin.W:
            pts.append((0, max(bin.W - dy0, 0), 0))
        if dy0 * 2 + gap <= bin.W:
            pts.append((0, dy0 + gap, 0))
            pts.append((0, max(bin.W - 2 * dy0 - gap, 0), 0))
        # CTU：中段柜长锚点（25%/40%/50%/60%/75%），避免只从门端 x=0 起铺
        if cog_on and bin.L > dx0:
            for frac in (0.25, 0.35, 0.45, 0.5, 0.55, 0.65, 0.75):
                ax = int(bin.L * frac - dx0 / 2.0)
                ax = max(0, min(ax, bin.L - dx0))
                pts.append((ax, 0, 0))
                if dy0 <= bin.W:
                    pts.append((ax, max(bin.W - dy0, 0), 0))
                if dy0 * 2 + gap <= bin.W:
                    pts.append((ax, dy0 + gap, 0))
        step = max(min(dx0 + max(gap, 1), 250), 100)
        x = 0
        while x + dx0 <= bin.L:
            _cancel.check()
            pts.append((x, 0, 0))
            if dy0 <= bin.W:
                pts.append((x, max(bin.W - dy0, 0), 0))
            if dy0 * 2 + gap <= bin.W:
                pts.append((x, dy0 + gap, 0))
            x += step
    # 已放箱：水平邻接 + 可承重箱顶面采样
    for p in bin.placements:
        pts.append((p.x + p.dx + gap, p.y, p.z))
        pts.append((p.x, p.y + p.dy + gap, p.z))
        pts.append((p.x + p.dx + gap, 0, 0))
        pts.append((p.x + p.dx + gap, max(bin.W - p.dy, 0), 0))
        top_z = p.z + p.dz
        can_top = bool(getattr(p, "stackable", True))
        if can_top and top_z + 50 <= max_zh:
            pts.append((p.x, p.y, top_z))
            pts.append((p.x + max(p.dx // 2, 0), p.y, top_z))
            pts.append((p.x, p.y + max(p.dy // 2, 0), top_z))
            pts.append((p.x + max(p.dx // 4, 0), p.y + max(p.dy // 4, 0), top_z))
            pts.append(
                (
                    p.x + max(p.dx - max(p.dx // 4, 1), 0),
                    p.y + max(p.dy // 4, 0),
                    top_z,
                )
            )
            pts.append((max(0, p.x + p.dx - 1), max(0, p.y + p.dy - 1), top_z))
    pts = list({p for p in pts})
    # 可叠：先试上层点；不可叠：底层优先
    if want_stack:
        pts.sort(key=lambda t: (0 if t[2] > 0 else 1, t[2], t[0], t[1]))
    else:
        pts.sort(key=lambda t: (t[2], t[0], t[1]))

    best: Optional[Tuple] = None
    best_meta: Optional[Tuple[int, int, int, int, int, int, int]] = None
    mid_y = bin.W / 2.0
    L = float(bin.L) if bin.L > 0 else 1.0
    lo_mid, hi_mid = 0.25 * L, 0.75 * L
    w = max(float(item.weight_kg), 1.0)
    # 已放箱的质量矩（横向 + 纵向）与 mid50 累计
    moment_y = 0.0
    m_tot = 0.0
    m_mid = 0.0
    mx_sum = 0.0
    mz_sum = 0.0
    for p in bin.placements:
        pw = float(getattr(p, "weight_kg", 0) or (p.dx * p.dy * p.dz / 1e8))
        if pw <= 0:
            pw = 1.0
        cx = p.x + p.dx / 2.0
        cy = p.y + p.dy / 2.0
        cz = p.z + p.dz / 2.0
        moment_y += pw * (cy - mid_y)
        m_tot += pw
        mx_sum += pw * cx
        mz_sum += pw * cz
        if lo_mid <= cx <= hi_mid:
            m_mid += pw

    for dx, dy, dz in _orientations(item):
        for px, py, pz in pts:
            if not _fits(bin, px, py, pz, dx, dy, dz):
                continue
            ok, layer, support_w = _stack_ok(
                bin,
                item,
                px,
                py,
                pz,
                dx,
                dy,
                dz,
                max_stack_height_mm=max_zh,
                max_stack_layers=global_layers,
            )
            if not ok:
                continue

            center_y = py + dy / 2.0
            center_x = px + dx / 2.0
            center_z = pz + dz / 2.0
            new_moment = abs(moment_y + w * (center_y - mid_y))
            y_to_wall = min(py, max(bin.W - (py + dy), 0))
            residual_h = max_zh - (pz + dz)
            residual_x = bin.L - (px + dx)
            # 重下轻上软偏好：上层重于支撑均值 → 罚分（不硬拒）
            heavy_on_light = 0.0
            if pz > 0:
                if support_w > 0:
                    heavy_on_light = max(0.0, float(item.weight_kg) - support_w) / max(
                        support_w, 1.0
                    )
                else:
                    heavy_on_light = 0.2

            # CTU 系统级 CoG：放置后 mid50 质量比 + 纵/横/竖向偏心
            mid50_pen = 0.0
            long_pen = 0.0
            height_pen = 0.0
            if cog_on:
                new_m = m_tot + w
                new_mid = m_mid + (w if lo_mid <= center_x <= hi_mid else 0.0)
                mid50_ratio = new_mid / new_m if new_m > 0 else 0.0
                # 目标 ≥60% 质量在中段 50%；再平衡模式加重
                rebal = bool(getattr(pol, "cog_rebalance", False))
                k_mid = 18.0 if rebal else 8.0
                k_out = 12.0 if rebal else 4.0
                mid50_pen = max(0.0, 0.60 - mid50_ratio) * k_mid
                # 重货在带外额外重罚
                w_factor = min(3.0, max(1.0, w / 200.0)) if rebal else 1.0
                if center_x < lo_mid:
                    mid50_pen += (lo_mid - center_x) / L * k_out * w_factor
                elif center_x > hi_mid:
                    mid50_pen += (center_x - hi_mid) / L * k_out * w_factor
                new_gx = (mx_sum + w * center_x) / new_m
                long_pos = new_gx / L
                long_pen = abs(long_pos - 0.5) * (6.0 if rebal else 3.0)
                if long_pos < 0.35 or long_pos > 0.65:
                    long_pen += 2.5 if rebal else 1.5
                if long_pos < 0.25 or long_pos > 0.75:
                    long_pen += 4.0 if rebal else 2.5
                new_gz = (mz_sum + w * center_z) / new_m
                h_ratio = new_gz / float(bin.H) if bin.H > 0 else 0.0
                height_pen = max(0.0, h_ratio - 0.55) * 4.0

            # 词典序
            if getattr(pol, "cog_rebalance", False) and cog_on:
                # 出运再平衡：mid50/long 优先，叠高仅软偏好
                cand = (
                    mid50_pen,
                    long_pen,
                    heavy_on_light,
                    0.0 if (want_stack and pz > 0) else 0.5,
                    new_moment * 0.01,
                    height_pen,
                    float(y_to_wall) * 0.001,
                    residual_h * 0.001,
                    float(px) * 0.00001,
                    0.0,
                    0.0,
                )
            elif want_stack:
                z_rank = 0.0 if pz > 0 else 1.0
                cand = (
                    z_rank,
                    float(layer) * 0.01,
                    heavy_on_light,
                    mid50_pen if cog_on else float(px) * 0.0001,
                    long_pen if cog_on else 0.0,
                    new_moment * 0.01,
                    height_pen if cog_on else 0.0,
                    float(y_to_wall) * 0.001,
                    residual_h * 0.001,
                    -float(residual_x) * 0.0001,
                    float(px) * 0.00001,
                )
            else:
                cand = (
                    float(pz),
                    heavy_on_light,
                    mid50_pen if cog_on else float(px) * 0.0001,
                    long_pen if cog_on else 0.0,
                    new_moment * 0.01,
                    height_pen if cog_on else 0.0,
                    float(y_to_wall) * 0.001,
                    residual_h * 0.001,
                    -float(residual_x) * 0.0001,
                    float(px) * 0.00001,
                    0.0,
                )
            if best is None or cand < best:
                best = cand
                best_meta = (dx, dy, dz, px, py, pz, layer)

    if best is None or best_meta is None:
        return None
    dx, dy, dz, x, y, z, layer = best_meta
    pl = Placement3D(
        box_id=item.box_id,
        x=int(x),
        y=int(y),
        z=int(z),
        dx=int(dx),
        dy=int(dy),
        dz=int(dz),
        container_no=bin.container_no,
        layer=int(layer),
        stackable=bool(item.stackable),
        max_stack_layers=item.max_stack_layers,
        max_top_load_kg=item.max_top_load_kg,
    )
    setattr(pl, "weight_kg", float(item.weight_kg or 0))
    bin.placements.append(pl)
    bin.add_weight(item.weight_kg)
    _add_points(bin, pl.x, pl.y, pl.z, pl.dx, pl.dy, pl.dz)
    return pl


def _strip_pack_floor(
    items: List[Item3D],
    *,
    L: int,
    W: int,
    H: int,
    max_load_kg: float,
    container_no: int,
) -> Tuple[List[Placement3D], List[Item3D], float]:
    """
    底层条带装填：长铁架/准同宽箱沿柜长两列并排向前推。
    EP 失败时的兜底，专治「明明 4 个 4m 架能进 1 个 40HQ」。
    """
    if not items:
        return [], [], 0.0
    # 仅处理可卧放的底层件；过高的跳过
    work = [it for it in items if min(it.dx, it.dy, it.dz) <= H]
    if not work:
        return [], list(items), 0.0

    placements: List[Placement3D] = []
    used_w = 0.0
    # 每列当前 x 前沿
    # 两列：y=0 与 y=col_w（若 2*min_width <= W）
    # 先按最长边作长度、次边作宽度
    def orient_floor(it: Item3D) -> Tuple[int, int, int]:
        # 高度尽量用原高（no_tip 时 dz 固定）
        oris = _orientations(it)
        # 选能进柜且底面积大、高度合法的
        best_o = None
        for dx, dy, dz in oris:
            if dx <= L and dy <= W and dz <= H:
                score = (dz <= H, -dx * dy, dz)  # 底层大底面积
                if best_o is None or score < best_o[0]:
                    best_o = (score, dx, dy, dz)
        if best_o is None:
            return it.dx, it.dy, it.dz
        return best_o[1], best_o[2], best_o[3]

    # 估列宽：取众数宽度
    widths = []
    for it in work:
        _, dy, _ = orient_floor(it)
        widths.append(dy)
    col_w = max(widths) if widths else W
    n_cols = 2 if col_w * 2 <= W else 1
    col_front = [0] * n_cols  # 各列已占用长度
    col_y = [0, col_w] if n_cols == 2 else [0]
    left: List[Item3D] = []
    # 长件优先
    ordered = sorted(work, key=lambda it: (-max(it.dx, it.dy, it.dz), -it.weight_kg))
    placed_ids = set()

    for it in ordered:
        if used_w + it.weight_kg > max_load_kg + 1e-6:
            left.append(it)
            continue
        dx, dy, dz = orient_floor(it)
        if dz > H or dy > W or dx > L:
            left.append(it)
            continue
        # 找能放下的列（前沿 + dx <= L，且该列宽够）
        best_c = None
        for c in range(n_cols):
            if dy > (W - col_y[c]) and c == n_cols - 1:
                # 最后一列用靠壁
                y = max(0, W - dy)
            else:
                y = col_y[c]
            x = col_front[c]
            if x + dx <= L and y + dy <= W:
                # 优先前沿更小的列（齐头并进可改：优先前沿更小使两侧齐）
                key = (col_front[c], c)
                if best_c is None or key < best_c[0]:
                    best_c = (key, c, x, y, dx, dy, dz)
        if best_c is None:
            left.append(it)
            continue
        _, c, x, y, dx, dy, dz = best_c
        pl = Placement3D(
            box_id=it.box_id,
            x=x,
            y=y,
            z=0,
            dx=dx,
            dy=dy,
            dz=dz,
            container_no=container_no,
            layer=1,
            stackable=bool(it.stackable),
            max_stack_layers=it.max_stack_layers,
        )
        setattr(pl, "weight_kg", float(it.weight_kg or 0))
        placements.append(pl)
        used_w += it.weight_kg
        col_front[c] = x + dx
        placed_ids.add(it.box_id)

    left.extend([it for it in items if it.box_id not in placed_ids and it not in left])
    # 去重 left
    seen = set()
    uniq_left = []
    for it in left:
        if it.box_id in placed_ids or it.box_id in seen:
            continue
        seen.add(it.box_id)
        uniq_left.append(it)
    return placements, uniq_left, used_w


def _grid_pack_uniform(
    items: List[Item3D],
    *,
    L: int,
    W: int,
    H: int,
    max_load_kg: float,
    container_no: int,
) -> Tuple[List[Placement3D], List[str], float]:
    """
    同尺寸/准同尺寸纸箱：层状网格装填（贴近网上「层数×每层件数」估算法）。
    返回 placements, unpacked_ids, used_weight。
    """
    if not items:
        return [], [], 0.0
    # 选体积最大朝向下的最优网格
    sample = items[0]
    best_cfg = None  # (nx, ny, nz, dx, dy, dz, per_layer, layers)
    for dx, dy, dz in _orientations(sample):
        if dx > L or dy > W or dz > H:
            continue
        nx = L // dx
        ny = W // dy
        nz = H // dz
        if nx < 1 or ny < 1 or nz < 1:
            continue
        score = (nx * ny * nz, nx * ny, -dz)  # 容量优先
        if best_cfg is None or score > best_cfg[0]:
            best_cfg = (score, nx, ny, nz, dx, dy, dz)
    if best_cfg is None:
        return [], [it.box_id for it in items], 0.0

    _, nx, ny, nz, dx, dy, dz = best_cfg
    # P0：不可叠则单层；尊重 policy/单箱 max_stack_layers 与限高
    pol = get_active_policy()
    max_zh = int(pol.max_stack_height_mm or H)
    max_zh = max(1, min(max_zh, H))
    max_layers = int(pol.max_stack_layers or 6)
    if sample.max_stack_layers is not None:
        max_layers = min(max_layers, int(sample.max_stack_layers))
    if not sample.stackable or sample.prefer_bottom:
        max_layers = 1
    nz = min(nz, max_layers, max(1, max_zh // max(dz, 1)))
    placements: List[Placement3D] = []
    used_w = 0.0
    unpacked: List[str] = []
    idx = 0
    n = len(items)

    # 坐标序：prefer_stack 时按「柱」装填（先叠满一柱再下一格），否则按层铺地
    cells: List[Tuple[int, int, int]] = []
    if pol.prefer_stack and sample.stackable and not sample.prefer_bottom and nz > 1:
        for iy in range(ny):
            for ix in range(nx):
                for iz in range(nz):
                    cells.append((ix, iy, iz))
    else:
        for iz in range(nz):
            for iy in range(ny):
                for ix in range(nx):
                    cells.append((ix, iy, iz))

    for ix, iy, iz in cells:
        if idx >= n:
            return placements, unpacked, used_w
        it = items[idx]
        if used_w + it.weight_kg > max_load_kg + 1e-6:
            unpacked.extend(x.box_id for x in items[idx:])
            return placements, unpacked, used_w
        # 重下轻上软序：同柱内尽量重的先放（items 已大体按重排序时自然）
        pl = Placement3D(
            box_id=it.box_id,
            x=ix * dx,
            y=iy * dy,
            z=iz * dz,
            dx=dx,
            dy=dy,
            dz=dz,
            container_no=container_no,
            layer=iz + 1,
            stackable=bool(it.stackable),
            max_stack_layers=it.max_stack_layers,
        )
        setattr(pl, "weight_kg", float(it.weight_kg or 0))
        placements.append(pl)
        used_w += it.weight_kg
        idx += 1
    if idx < n:
        unpacked.extend(x.box_id for x in items[idx:])
    return placements, unpacked, used_w


def _is_near_uniform(items: List[Item3D], tol: float = 0.08) -> bool:
    if len(items) < 8:
        return False
    vols = [it.dx * it.dy * it.dz for it in items]
    base = max(vols[0], 1)
    return all(abs(v - base) / base <= tol for v in vols)


def pack_items(
    items: List[Item3D],
    container_type: str = "40HQ",
    max_containers: int = 1,
    packing_options: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    ctype = container_type if container_type in CONTAINER_INNER else "40HQ"
    spec = CONTAINER_INNER[ctype]
    max_c = max(1, int(max_containers or 1))
    # 注入堆码策略（max_stack_height_mm / max_stack_layers / prefer_stack / clearance …）
    prev_pol = get_active_policy()
    set_active_policy(policy_from_options(packing_options))
    try:
        return _pack_items_core(
            items,
            container_type=ctype,
            max_containers=max_c,
            spec=spec,
        )
    finally:
        set_active_policy(prev_pol)


def _pack_items_core(
    items: List[Item3D],
    *,
    container_type: str,
    max_containers: int,
    spec: Dict[str, float],
) -> Dict[str, Any]:
    ctype = container_type
    max_c = max_containers
    bins: List[Bin3D] = []
    unpacked: List[str] = []
    layout: List[Dict[str, Any]] = []

    def new_bin(no: int) -> Bin3D:
        b = Bin3D(
            L=int(spec["L"]),
            W=int(spec["W"]),
            H=int(spec["H"]),
            max_load_kg=float(spec["max_load_kg"]),
            container_no=no,
        )
        b._used_weight = 0.0
        return b

    remaining = list(items)
    pol = get_active_policy()

    def _append_pls(b: Bin3D, pls: List[Placement3D]) -> None:
        for pl in pls:
            layout.append(
                {
                    "box_id": pl.box_id,
                    "container_no": pl.container_no,
                    "position": {"x": pl.x, "y": pl.y, "z": pl.z},
                    "size": {"dx": pl.dx, "dy": pl.dy, "dz": pl.dz},
                    "rotation": "LWH",
                    "layer": pl.layer,
                }
            )

    # P0：多柜 → 优先重量配额分柜（先于网格/条带，避免最差柜被轻货堆满）
    # 大票（≥4 柜或 ≥20 箱）即使未开 cog 也启用；均匀小箱走网格路径
    use_weight_balance_early = (
        max_c >= 2
        and len(remaining) >= 6
        and (
            bool(getattr(pol, "cog_aware", False))
            or max_c >= 4
            or len(remaining) >= 20
        )
        and not _is_near_uniform(remaining)
    )
    if use_weight_balance_early:
        import math

        total_w = sum(float(it.weight_kg or 0) for it in remaining)
        cap = float(spec["max_load_kg"])
        # 目标柜数：重量下界与 max_c 取紧；填充目标 0.88 提高重量利用率
        fill_tgt = 0.88 if max_c >= 6 or len(remaining) >= 30 else 0.92
        n_need = max(1, min(max_c, int(math.ceil(total_w / max(cap * fill_tgt, 1)))))
        # 几何：若 max_c 明显大于重量下界，仍按重量分柜再 spill，避免一上来开满 max_c
        groups: List[List[Item3D]] = [[] for _ in range(n_need)]
        loads = [0.0] * n_need
        for it in sorted(remaining, key=lambda x: -float(x.weight_kg or 0)):
            _cancel.check()
            cands = [
                (loads[i], i)
                for i in range(n_need)
                if loads[i] + float(it.weight_kg or 0) <= cap + 1e-6
            ]
            if cands:
                _, bi = min(cands)
            else:
                bi = min(range(n_need), key=lambda i: loads[i])
            groups[bi].append(it)
            loads[bi] += float(it.weight_kg or 0)

        def _append_layout(pl: Placement3D) -> None:
            layout.append(
                {
                    "box_id": pl.box_id,
                    "container_no": pl.container_no,
                    "position": {"x": pl.x, "y": pl.y, "z": pl.z},
                    "size": {"dx": pl.dx, "dy": pl.dy, "dz": pl.dz},
                    "rotation": "LWH",
                    "layer": pl.layer,
                }
            )

        def _place_or_spill(item: Item3D, preferred: Optional[Bin3D] = None) -> bool:
            """在 preferred/已有柜/新柜(≤max_c) 放置；绝不突破 max_c。"""
            order: List[Bin3D] = []
            if preferred is not None:
                order.append(preferred)
            order.extend(b2 for b2 in bins if b2 is not preferred)
            for b2 in order:
                pl2 = try_place(b2, item)
                if pl2:
                    _append_layout(pl2)
                    return True
            if len(bins) < max_c:
                b3 = new_bin(len(bins) + 1)
                bins.append(b3)
                pl3 = try_place(b3, item)
                if pl3:
                    _append_layout(pl3)
                    return True
            unpacked.append(item.box_id)
            return False

        for gi, grp in enumerate(groups):
            if not grp:
                continue
            # 关键：每组开柜必须受 max_c 约束（spill 已占满时不得再 append）
            preferred: Optional[Bin3D] = None
            if len(bins) < max_c:
                preferred = new_bin(len(bins) + 1)
                bins.append(preferred)
            elif bins:
                preferred = min(
                    bins, key=lambda x: float(getattr(x, "_used_weight", 0) or 0)
                )
            grp_ord = sorted(
                grp,
                key=lambda it: (
                    0 if it.prefer_bottom else 1,
                    -float(it.weight_kg or 0),
                    -(it.dx * it.dy),
                ),
            )
            for item in grp_ord:
                _cancel.check()
                _place_or_spill(item, preferred)
        remaining = []
        # 跳过网格/条带预装与二次配额

    # 大量同尺寸纸箱：整柜网格装填（对齐网上 50×40×35cm 类案例）
    if remaining and _is_near_uniform(remaining) and len(remaining) >= 12:
        left = list(remaining)
        while left and len(bins) < max_c:
            b = new_bin(len(bins) + 1)
            pls, _unp, used_w = _grid_pack_uniform(
                left,
                L=b.L,
                W=b.W,
                H=b.H,
                max_load_kg=b.max_load_kg,
                container_no=b.container_no,
            )
            if not pls:
                break
            b.placements = pls
            b._used_weight = used_w
            bins.append(b)
            _append_pls(b, pls)
            packed_ids = {p.box_id for p in pls}
            left = [it for it in left if it.box_id not in packed_ids]
        remaining = left
        if remaining and len(bins) >= max_c:
            unpacked.extend(it.box_id for it in remaining)
            remaining = []

    # 混 SKU：按尺寸分组后，尽量用「层状网格占用剩余空间」；不足再 EP
    if remaining and not _is_near_uniform(remaining) and len(remaining) >= 20:
        from collections import defaultdict

        groups: Dict[Tuple[int, int, int], List[Item3D]] = defaultdict(list)
        for it in remaining:
            key = tuple(sorted((it.dx, it.dy, it.dz)))
            groups[key].append(it)
        # 大体积组优先
        group_list = sorted(groups.values(), key=lambda g: -(g[0].dx * g[0].dy * g[0].dz * len(g)))
        still: List[Item3D] = []
        if not bins and max_c >= 1:
            bins.append(new_bin(1))
        for g in group_list:
            # 尝试在已有柜用 EP 快速塞；组很大时对空柜做网格
            if len(g) >= 24 and len(bins) < max_c and all(len(b.placements) == 0 for b in bins[-1:]):
                b = bins[-1] if bins and not bins[-1].placements else new_bin(len(bins) + 1)
                if b not in bins:
                    bins.append(b)
                pls, unp_ids, used_w = _grid_pack_uniform(
                    g,
                    L=b.L,
                    W=b.W,
                    H=b.H,
                    max_load_kg=b.max_load_kg - float(getattr(b, "_used_weight", 0)),
                    container_no=b.container_no,
                )
                if pls and not b.placements:
                    b.placements = pls
                    b._used_weight = used_w
                    _append_pls(b, pls)
                    packed = {p.box_id for p in pls}
                    still.extend([it for it in g if it.box_id not in packed])
                    continue
            still.extend(g)
        remaining = still

    pol = get_active_policy()
    # 底层件优先，再重货，再可上二层的轻箱；同层内长件优先（利于条带齐头）
    # cog_rebalance / cog_aware：重货更优先（中段 EP）
    if pol.cog_rebalance or pol.cog_aware:
        ordered = sorted(
            remaining,
            key=lambda it: (
                0 if it.prefer_bottom else 1,
                -float(it.weight_kg or 0),
                0 if not it.stackable else 1,
                -max(it.dx, it.dy, it.dz),
                -(it.dx * it.dy),
            ),
        )
    else:
        ordered = sorted(
            remaining,
            key=lambda it: (
                0 if it.prefer_bottom else 1,
                0 if not it.stackable else 1,
                -max(it.dx, it.dy, it.dz),
                -it.weight_kg,
                -(it.dx * it.dy),
            ),
        )

    def _is_frame_like(it: Item3D) -> bool:
        """铁架/底层长货：条带优先，避免与小箱混排碎片。

        P0：可叠矮箱不要进条带（条带仅 z=0），否则永远叠不起来。
        """
        # 可上上层 → 交给 EP 叠高
        if it.stackable and not it.prefer_bottom:
            return False
        m = max(it.dx, it.dy, it.dz)
        # 1.1m 立方架 H≈1750、2m/4m 长架、显式底层
        return bool(
            it.prefer_bottom
            or m >= 2000
            or (it.dz >= 1500 and max(it.dx, it.dy) <= 4500 and min(it.dx, it.dy) >= 900)
        )

    def _commit_bin_placements(b: Bin3D, pls: List[Placement3D]) -> None:
        b.placements = list(pls)
        b.points = [(0, 0, 0)]
        for pl in pls:
            _add_points(b, pl.x, pl.y, pl.z, pl.dx, pl.dy, pl.dz)
            layout.append(
                {
                    "box_id": pl.box_id,
                    "container_no": pl.container_no,
                    "position": {"x": pl.x, "y": pl.y, "z": pl.z},
                    "size": {"dx": pl.dx, "dy": pl.dy, "dz": pl.dz},
                    "rotation": "LWH",
                    "layer": pl.layer,
                }
            )

    # 阶段 A：铁架/底层件先条带贴端墙（即使混有大量五金小箱也先清架）
    # 按 max_c 均分重量上限，避免第一柜吃满 PAYLOAD 把剩余货挤到第 3 柜
    frames = [it for it in ordered if _is_frame_like(it)]
    to_ep: List[Item3D] = list(ordered)
    if len(frames) >= 2 and max_c >= 1:
        left_strip = list(frames)
        strip_packed_ids: set = set()
        frame_w = sum(float(it.weight_kg or 0) for it in frames)
        # 预留约 15% 重量给板箱/五金填缝
        bal_cap = min(
            float(spec["max_load_kg"]) * 0.88,
            max(frame_w / max(max_c, 1) * 1.12, float(spec["max_load_kg"]) * 0.55),
        )
        while left_strip and len(bins) < max_c:
            b = new_bin(len(bins) + 1)
            bins_left = max_c - len(bins)
            # 最后一柜可用满载；前面的柜用均衡上限
            cap = float(spec["max_load_kg"]) if bins_left <= 1 else bal_cap
            pls, left_strip, used_w = _strip_pack_floor(
                left_strip,
                L=b.L,
                W=b.W,
                H=b.H,
                max_load_kg=cap,
                container_no=b.container_no,
            )
            if not pls:
                break
            b._used_weight = used_w
            bins.append(b)
            _commit_bin_placements(b, pls)
            strip_packed_ids.update(p.box_id for p in pls)
            if used_w <= 0:
                break
        # 未装入的架 + 全部非架 → EP 填缝（优先塞已有柜）
        to_ep = [it for it in ordered if it.box_id not in strip_packed_ids]
        if not bins:
            to_ep = list(ordered)

    # 阶段 B：EP 填缝 / 非长架货 / 条带剩余
    for item in to_ep:
        placed = None
        for b in bins:
            placed = try_place(b, item)
            if placed:
                break
        if not placed and len(bins) < max_c:
            bins.append(new_bin(len(bins) + 1))
            placed = try_place(bins[-1], item)
        if not placed:
            unpacked.append(item.box_id)
        else:
            layout.append(
                {
                    "box_id": placed.box_id,
                    "container_no": placed.container_no,
                    "position": {"x": placed.x, "y": placed.y, "z": placed.z},
                    "size": {"dx": placed.dx, "dy": placed.dy, "dz": placed.dz},
                    "rotation": "LWH",
                    "layer": placed.layer,
                }
            )

    # 容积：每个铁箱/木箱/铁笼按外廓「实心长方体」体积 = dx×dy×dz（mm³）
    # 不是零件净体积，也不是镂空骨架体积
    used_vol = sum(
        float(p["size"]["dx"]) * float(p["size"]["dy"]) * float(p["size"]["dz"]) for p in layout
    )
    cont_vol = float(spec["L"]) * float(spec["W"]) * float(spec["H"])
    total_w = sum(it.weight_kg for it in items if it.box_id not in unpacked)
    used_bins = len(bins) if bins else 0
    denom_vol = cont_vol * max(used_bins, 1)
    space_util = min(used_vol / denom_vol, 1.0) if denom_vol else 0.0
    weight_util = min(total_w / (spec["max_load_kg"] * max(used_bins, 1)), 9.99)

    # 分柜指标 + 底面积利用率（更贴近“好不好放”）
    per_container = []
    for b in bins:
        bvol = sum(float(p.dx) * float(p.dy) * float(p.dz) for p in b.placements)
        floor = sum(float(p.dx) * float(p.dy) for p in b.placements if p.z == 0)
        per_container.append(
            {
                "container_no": b.container_no,
                "volume_utilization": round(min(bvol / cont_vol, 1.0), 4) if cont_vol else 0,
                "floor_utilization": round(
                    min(floor / (spec["L"] * spec["W"]), 1.0), 4
                )
                if spec["L"] and spec["W"]
                else 0,
                "boxes": len(b.placements),
                "load_kg": round(float(getattr(b, "_used_weight", 0)), 1),
                "solid_volume_m3": round(bvol / 1e9, 4),
            }
        )
    # 主指标：若只有一柜装不下才开多柜，展示「最满一柜」与「平均」
    best_vol = max((c["volume_utilization"] for c in per_container), default=space_util)
    avg_floor = (
        sum(c["floor_utilization"] for c in per_container) / len(per_container)
        if per_container
        else 0
    )

    # 必须同时：装完 + 用柜数 ≤ max_c（防 weight_balance 等路径越权开柜）
    can_fit = (
        len(unpacked) == 0
        and len(layout) > 0
        and used_bins <= max_c
    )
    message = "可以顺利装下" if can_fit else f"未完全装入: {', '.join(unpacked[:20])}" + (
        f"…共{len(unpacked)}件" if len(unpacked) > 20 else ""
    )
    if used_bins > max_c and len(unpacked) == 0:
        message = f"用柜{used_bins}>上限{max_c}（视为未在预算内装下）"
    cargo_m3 = used_vol / 1e9
    cont_m3 = cont_vol / 1e9
    note = (
        f"摆柜几何容积率=Σ(箱外廓AABB)÷柜内几何容积"
        f"（已装 {cargo_m3:.3f} m³ / 单柜 {cont_m3:.3f} m³ ×{max(used_bins,1)}）；"
        f"最满柜 {best_vol:.0%}，底面积均 {avg_floor:.0%}。"
        f"此指标用于已成箱3D摆位，不等于材料估柜体积；"
        f"估柜请用 volume_estimate.pack_effective（件体积×货种膨胀≤1.8）。"
        f"钢结构常重量先满、此外廓容积率偏低属正常。"
    )
    pol = get_active_policy()
    stacked_n = sum(1 for p in layout if int((p.get("position") or {}).get("z") or 0) > 0)
    max_z = max(
        (int((p.get("position") or {}).get("z") or 0) for p in layout),
        default=0,
    )
    result = {
        "container_type": ctype,
        "containers_used": used_bins,
        "space_utilization": round(space_util, 4),
        "weight_utilization": round(weight_util, 4),
        "space_utilization_best_container": round(best_vol, 4),
        "floor_utilization_avg": round(avg_floor, 4),
        "per_container": per_container,
        "metrics_note": note,
        "volume_basis": "solid_outer_aabb",
        "volume_basis_note": "摆柜几何用外廓；材料估柜用 pack_effective，勿混用",
        "cargo_solid_volume_mm3": round(used_vol, 1),
        "cargo_solid_volume_m3": round(cargo_m3, 4),
        "container_inner_volume_m3": round(cont_m3, 4),
        "can_fit": can_fit,
        "layout": layout,
        "unpacked_box_ids": unpacked,
        "message": message,
        "engine": "python-laff-3d",
        "stacking": {
            "stacked_placements": stacked_n,
            "max_z_mm": max_z,
            "max_stack_height_mm": int(pol.max_stack_height_mm or spec["H"]),
            "max_stack_layers": int(pol.max_stack_layers),
            "prefer_stack": bool(pol.prefer_stack),
            "clearance_mm": int(pol.clearance_mm),
            "support_ratio_min": float(pol.support_ratio_min),
            "export_strict": bool(pol.export_strict),
            "cog_aware": bool(pol.cog_aware),
            "corner_support": bool(pol.corner_support),
        },
    }
    return result


def pack_boxes_api(
    boxes: Sequence[Dict[str, Any]],
    *,
    container_type: str = "40HQ",
    max_containers: int = 1,
    priority_order: Optional[List[str]] = None,
    packing_options: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """从 api-spec boxes[] 装载。packing_options 控制堆码限高/限层等。"""
    pol = policy_from_options(packing_options)
    items: List[Item3D] = []
    for b in boxes:
        outer = b.get("outer_size_mm") or {}
        special = b.get("special_attributes") or []
        L = int(round(float(outer.get("length") or 1)))
        W = int(round(float(outer.get("width") or 1)))
        H = int(round(float(outer.get("height") or 1)))
        btype = str(b.get("box_type") or "")
        # 超长/内容物超长：不任意 6 向转；铁架/笼默认禁止竖放
        longish = (
            "超长" in special
            or "内容物超长" in special
            or L >= 4000
            or float(b.get("content_max_length_mm") or 0) >= 4000
        )
        frameish = any(k in btype for k in ("铁架", "铁笼", "框", "木箱"))
        if b.get("allowRotate") is False:
            allow_rotate = False
            no_tip = True
        else:
            no_tip = frameish or longish or L >= 2000
            allow_rotate = True
        stackable = bool(b.get("stackable"))
        if "stackable" not in b:
            stackable = H <= int(pol.stackable_max_box_h_mm) and not longish
        gross = float(b.get("gross_weight_kg") or 0)
        # prefer_bottom：显式 / 超长 / 超重阈值（阈值来自 packing_options）
        prefer_bottom = bool(b.get("prefer_bottom")) or longish or (
            gross >= float(pol.prefer_bottom_weight_kg)
        )
        # 单箱层数上限（可选）
        msl = b.get("max_stack_layers")
        try:
            msl_i = int(msl) if msl is not None else None
        except Exception:
            msl_i = None
        if msl_i is not None:
            msl_i = max(1, min(6, msl_i))
        mtl = b.get("max_top_load_kg")
        try:
            mtl_f = float(mtl) if mtl is not None else None
        except Exception:
            mtl_f = None
        if mtl_f is None and gross > 0:
            # 默认顶面承重 ≈ 1.5× 自身毛重（铁箱可更高由业务覆盖）
            mtl_f = gross * float(pol.default_top_load_factor)
        # stackable=false → 不上上层 + 顶面不承重
        # prefer_bottom → 仅禁止上上层，顶面仍可按 stackable 承重（重下轻上）
        items.append(
            Item3D(
                box_id=str(b.get("box_id") or ""),
                dx=max(L, 1),
                dy=max(W, 1),
                dz=max(H, 1),
                weight_kg=gross,
                allow_rotate=allow_rotate,
                no_tip=no_tip,
                stackable=bool(stackable),
                prefer_bottom=prefer_bottom,
                max_stack_layers=msl_i,
                max_top_load_kg=mtl_f,
            )
        )

    if priority_order:
        order = {bid: i for i, bid in enumerate(priority_order)}
        items.sort(key=lambda it: order.get(it.box_id, 999))

    base_opts = dict(packing_options or {})
    # P2 multi-start：叠高/地面 + 重底序 多策略，CoG/空隙选优
    # 大票裁剪：>100 件只跑 2 候选（default+mid_heavy），先 R 后少 multi，控时
    do_multi = bool(pol.multi_start) and len(items) >= 4
    multi_budget = "full"
    if len(items) > 120:
        multi_budget = "tiny"  # 1+1
    elif len(items) > 60:
        multi_budget = "mid"  # 3

    def _finish(p: Dict[str, Any], tag: str = "default") -> Dict[str, Any]:
        p = _attach_plan_cog(p, boxes, container_type)
        p = _attach_layout_quality(p, boxes)
        try:
            from packing_assistant.tools.cog_shift import apply_r0_r1
            from packing_assistant.tools.cog_repair import apply_r4_repair

            from packing_assistant.tools.cog_slab import apply_r2_slab_reorder, apply_r3_partial_repack
            from packing_assistant.tools.cog_lns import apply_lns_worst_container
            from packing_assistant.tools.cog_lateral import apply_lateral_repair

            tgt = float(base_opts.get("r4_target_mid50") or base_opts.get("mid50_target") or 0.60)
            force_cog = bool(
                base_opts.get("cog_rebalance", True) or base_opts.get("r1_force")
            )

            # R0/R1 → R2 → R4 → R3 → LNS最差柜 → 横偏修理 → R0/R1 收口
            # 默认 force_cog=True：装完必须抬 mid50，避免 CoG=block
            do_r01 = bool(base_opts.get("r0_r1", True) or base_opts.get("r1_shift", True))
            if do_r01:
                p = apply_r0_r1(
                    p,
                    boxes,
                    force=force_cog,
                    enable_mirror=bool(base_opts.get("r1_mirror", True)),
                    enable_shift=bool(base_opts.get("r1_shift", True)),
                )
            do_r2 = bool(base_opts.get("r2_slab", True) or force_cog)
            if do_r2:
                p = apply_r2_slab_reorder(
                    p, boxes, n_slabs=int(base_opts.get("r2_n_slabs") or 6),
                    target_mid50=tgt, force=force_cog,
                )
            do_r4 = bool(base_opts.get("r4_repair", True) or force_cog)
            if do_r4:
                p = apply_r4_repair(p, boxes, target_mid50=tgt, force=True)
            do_r3 = bool(base_opts.get("r3_repack", True) or force_cog)
            if do_r3:
                p = apply_r3_partial_repack(p, boxes, target_mid50=tgt)
            do_lns = bool(base_opts.get("lns_worst", True) or force_cog)
            if do_lns:
                p = apply_lns_worst_container(p, boxes, target_mid50=tgt, force=force_cog)
            do_lat = bool(base_opts.get("lateral_repair", True) or force_cog)
            if do_lat:
                p = apply_lateral_repair(
                    p, boxes,
                    lat_threshold=float(base_opts.get("lat_threshold") or 0.08),
                    force=force_cog,
                )
            if do_r01:
                p = apply_r0_r1(p, boxes, force=False)
            # 收口再强制 R4 一次，确保 mid50 目标
            if do_r4:
                p = apply_r4_repair(p, boxes, target_mid50=tgt, force=True)
            p = _attach_layout_quality(p, boxes)
            p = _attach_plan_cog(p, boxes, container_type)
        except Exception as _cog_ex:
            # 不整段吞掉：至少尝试 R4 + 重算 CoG
            try:
                from packing_assistant.tools.cog_repair import apply_r4_repair as _r4

                p = _r4(
                    p,
                    boxes,
                    target_mid50=float(
                        base_opts.get("r4_target_mid50")
                        or base_opts.get("mid50_target")
                        or 0.60
                    ),
                    force=True,
                )
                p = _attach_plan_cog(p, boxes, container_type)
                st_err = dict(p.get("stacking") or {})
                st_err["cog_pipeline_error"] = f"{type(_cog_ex).__name__}: {_cog_ex}"
                p["stacking"] = st_err
            except Exception:
                pass
        st0 = dict(p.get("stacking") or {})
        st0["candidate_tag"] = tag
        p = {**p, "stacking": st0}
        return p

    def _run(it_list: List[Item3D], extra: Dict[str, Any], tag: str) -> Dict[str, Any]:
        p = pack_items(
            it_list,
            container_type=container_type,
            max_containers=max_containers,
            packing_options={**base_opts, "multi_start": False, **extra},
        )
        return _finish(p, tag)

    # 单策略路径也要 R1
    if not do_multi:
        p0 = pack_items(
            items,
            container_type=container_type,
            max_containers=max_containers,
            packing_options={**base_opts, "multi_start": False},
        )
        return _finish(p0, "default")

    candidates: List[Tuple[str, Dict[str, Any]]] = []
    candidates.append(
        (
            "stack" if pol.prefer_stack else "floor",
            _run(list(items), {}, "default"),
        )
    )
    if do_multi:
        # 大票：只补 mid_heavy（最贴 CTU 中段）
        mid_heavy = sorted(
            items,
            key=lambda it: (
                0 if it.prefer_bottom else 1,
                -float(it.weight_kg or 0),
                -(it.dx * it.dy * it.dz),
            ),
        )
        candidates.append(
            (
                "mid_heavy",
                _run(
                    mid_heavy,
                    {
                        "prefer_stack": pol.prefer_stack,
                        "cog_aware": True,
                        "cog_rebalance": True,
                    },
                    "mid_heavy",
                ),
            )
        )
        if multi_budget in ("mid", "full"):
            candidates.append(
                (
                    "floor" if pol.prefer_stack else "stack",
                    _run(
                        list(items),
                        {"prefer_stack": not pol.prefer_stack},
                        "alt_stack",
                    ),
                )
            )
        if multi_budget == "full":
            heavy_first = sorted(
                items, key=lambda it: (-float(it.weight_kg or 0), -it.dx * it.dy)
            )
            candidates.append(
                (
                    "heavy_first",
                    _run(heavy_first, {"prefer_stack": pol.prefer_stack}, "heavy_first"),
                )
            )
            if bool(base_opts.get("cog_rebalance")) or bool(pol.cog_aware):
                by_w = sorted(items, key=lambda it: -float(it.weight_kg or 0))
                n_h = max(1, int(len(by_w) * 0.6))
                rebal_order = by_w[:n_h] + list(reversed(by_w[n_h:]))
                candidates.append(
                    (
                        "cog_rebalance",
                        _run(
                            rebal_order,
                            {
                                "prefer_stack": True,
                                "cog_aware": True,
                                "cog_rebalance": True,
                                "clearance_mm": int(
                                    base_opts.get("clearance_mm") or pol.clearance_mm
                                ),
                            },
                            "cog_rebalance",
                        ),
                    )
                )

    def _score(p: Dict[str, Any]) -> Tuple:
        used = int(p.get("containers_used") or 99)
        fit = 0 if p.get("can_fit") else 1
        util = -float(p.get("space_utilization") or 0)
        st = -int((p.get("stacking") or {}).get("stacked_placements") or 0)
        unp = int(len(p.get("unpacked_box_ids") or []))
        # 用 worst 柜 mid50（出运决策）
        bundle = p.get("cog_bundle") or {}
        cog = p.get("cog") or bundle.get("worst") or bundle.get("primary") or {}
        bal = str(cog.get("balance") or "")
        bal_rank = {"ok": 0, "warn": 1, "warn_high": 2, "block": 3}.get(bal, 2)
        mid50 = float(
            bundle.get("worst_mid50")
            if bundle.get("worst_mid50") is not None
            else (cog.get("mass_in_mid50_ratio") or 0.0)
        )
        # mid50 硬优先：低于 0.4 大罚，低于 0.6 中罚
        if mid50 < 0.40:
            mid50_pen = 10.0 + (0.40 - mid50) * 20.0
        else:
            mid50_pen = max(0.0, 0.60 - mid50) * 8.0
        long_pos = float(cog.get("longitudinal_position") or 0.5)
        long_dev = abs(long_pos - 0.5)
        h_ratio = float(cog.get("height_ratio") or 0.0)
        height_pen = max(0.0, h_ratio - 0.55)
        lat = float(cog.get("lateral_eccentricity") or 0.0)
        lq = p.get("layout_quality") or {}
        void_pen = float(lq.get("gaps_over_limit") or 0) * 0.5 + max(
            0.0, float(lq.get("max_horizontal_gap_mm") or 0) - 150.0
        ) * 0.001
        return (
            fit,
            used,
            unp,
            mid50_pen,  # 出运：mid50 优先于 balance 细项
            bal_rank,
            long_dev,
            height_pen,
            lat,
            void_pen,
            util,
            st,
        )

    best_tag, best = candidates[0]
    best_sc = _score(best)
    for tag, p in candidates[1:]:
        sc = _score(p)
        if sc < best_sc:
            best_sc = sc
            best = p
            best_tag = tag

    st = dict(best.get("stacking") or {})
    st["multi_start_winner"] = best_tag
    st["multi_start_cog"] = True
    st["multi_start_n"] = len(candidates)
    eng = str(best.get("engine") or "python-laff-3d")
    if len(candidates) > 1 and "+multi" not in eng:
        eng = eng + "+multi"
    best = {**best, "stacking": st, "engine": eng}
    if "layout_quality" not in best:
        best = _attach_layout_quality(best, boxes)
    # R1 已在 _finish / 各候选内做过；此处仅补全指标
    return best


def _attach_plan_cog(
    plan: Dict[str, Any],
    boxes: Sequence[Dict[str, Any]],
    container_type: str,
) -> Dict[str, Any]:
    """把主柜 CoG 摘要挂到 plan（供 multi_start / evaluator / risk）。"""
    try:
        from packing_assistant.tools.cog import cog_for_layout, compute_cog_bundle

        layout = plan.get("layout") or []
        if not layout:
            return plan
        bundle = compute_cog_bundle(plan, boxes=boxes)
        # 出运/展示：用最差柜（mid50 最低），不是柜1
        primary = None
        if bundle:
            primary = bundle.get("worst") or bundle.get("primary")
        if primary is None:
            primary = cog_for_layout(
                layout,
                container_type=str(plan.get("container_type") or container_type),
                boxes=list(boxes),
                container_no=1,
            )
        out = dict(plan)
        if primary:
            out["cog"] = primary
        if bundle:
            out["cog_bundle"] = bundle
            out["worst_mid50"] = bundle.get("worst_mid50")
            out["all_mid50_ok"] = bundle.get("all_mid50_ok")
        return out
    except Exception:
        return plan


def _attach_layout_quality(
    plan: Dict[str, Any],
    boxes: Sequence[Dict[str, Any]],
) -> Dict[str, Any]:
    try:
        from packing_assistant.tools.layout_quality import analyze_layout_quality

        lq = analyze_layout_quality(plan, boxes, void_limit_mm=150.0)
        out = dict(plan)
        out["layout_quality"] = lq
        return out
    except Exception:
        return plan
