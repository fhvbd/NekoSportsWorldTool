//! 轨迹几何与随机工具。
//!
//! round 封装、RNG、打卡点 Catmull-Rom 拟合环 + 弧长表 + 弧长插值。

use chrono::{Local, TimeZone};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};

pub const MET_PER_DEG_LAT: f64 = 111_132.0;
pub const MET_PER_DEG_LNG: f64 = 86_600.0;

/// round(x, n)：按精确二进制值四舍五入。
pub fn round_to(x: f64, n: usize) -> f64 {
    let s = format!("{x:.n$}");
    s.parse().unwrap_or(x)
}

/// 可播种 RNG 封装。
pub struct Rng {
    inner: StdRng,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            inner: StdRng::seed_from_u64(seed),
        }
    }
    pub fn random(&mut self) -> f64 {
        rand::Rng::gen_range(&mut self.inner, 0.0..1.0)
    }
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        rand::Rng::gen_range(&mut self.inner, a..=b)
    }
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        rand::Rng::gen_range(&mut self.inner, a..=b)
    }
    pub fn gauss(&mut self, mu: f64, sigma: f64) -> f64 {
        Normal::new(mu, sigma).unwrap().sample(&mut self.inner)
    }
    pub fn choice<T>(&mut self, items: &[T]) -> T
    where
        T: Copy,
    {
        items[rand::Rng::gen_range(&mut self.inner, 0..items.len())]
    }
    pub fn weighted<T>(&mut self, items: &[(T, u32)]) -> T
    where
        T: Copy,
    {
        let total: u32 = items.iter().map(|(_, w)| *w).sum();
        let mut u = self.uniform(0.0, total as f64);
        for (item, w) in items {
            u -= *w as f64;
            if u < 0.0 {
                return *item;
            }
        }
        items[items.len() - 1].0
    }
}

/// 弧长表：dense 相邻点距离的累加，长度为 dense.len() + 1。
pub fn arc_table(dense: &[(f64, f64)]) -> Vec<f64> {
    let mut arcs = vec![0.0f64];
    for i in 1..=dense.len() {
        let a = dense[i - 1];
        let b = dense[i % dense.len()];
        arcs.push(arcs[i - 1] + ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt());
    }
    arcs
}

fn catmull_rom(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), t: f64) -> (f64, f64) {
    let (t2, t3) = (t * t, t * t * t);
    let x = 0.5
        * ((2.0 * p1.0)
            + (-p0.0 + p2.0) * t
            + (2.0 * p0.0 - 5.0 * p1.0 + 4.0 * p2.0 - p3.0) * t2
            + (-p0.0 + 3.0 * p1.0 - 3.0 * p2.0 + p3.0) * t3);
    let y = 0.5
        * ((2.0 * p1.1)
            + (-p0.1 + p2.1) * t
            + (2.0 * p0.1 - 5.0 * p1.1 + 4.0 * p2.1 - p3.1) * t2
            + (-p0.1 + 3.0 * p1.1 - 3.0 * p2.1 + p3.1) * t3);
    (x, y)
}

/// 手绘折线（BD 系）→ 闭合平面路径 + 弧长表 + 中心。
///
/// 与 [`make_point_ring`] 的区别：顶点顺序**完全按用户画的样子**（不再按质心角排序），
/// 段间用 Catmull-Rom 平滑（手绘折线转角太硬，真人轨迹是圆滑的），
/// 每段采样数按长度自适应（≈1.5 m 一个密集点），弧长插值才够准。
///
/// `closed=false` 时按「原路折返」闭合成环：A→B→C→B→A，
/// 这样往返跑与单圈跑都能用同一个环形模型。
pub fn make_path_ring(path_bd: &[(f64, f64)], closed: bool) -> PointRing {
    let n = path_bd.len();
    let cx = path_bd.iter().map(|q| q.0).sum::<f64>() / n as f64;
    let cy = path_bd.iter().map(|q| q.1).sum::<f64>() / n as f64;
    let plane: Vec<(f64, f64)> = path_bd
        .iter()
        .map(|q| ((q.1 - cy) * MET_PER_DEG_LNG, (q.0 - cx) * MET_PER_DEG_LAT))
        .collect();

    // ① 折返：把回程点接在末尾（去掉重复的终点）
    let mut pts = plane.clone();
    if !closed {
        for p in plane.iter().rev().skip(1) {
            pts.push(*p);
        }
    }

    // ② 去掉过近的相邻点（<0.5 m），避免 Catmull-Rom 出现尖角/除零
    let mut cleaned: Vec<(f64, f64)> = Vec::with_capacity(pts.len());
    for p in pts {
        if let Some(last) = cleaned.last() {
            let d = ((p.0 - last.0).powi(2) + (p.1 - last.1).powi(2)).sqrt();
            if d < 0.5 {
                continue;
            }
        }
        cleaned.push(p);
    }
    if closed && cleaned.len() >= 2 {
        // 闭环时首尾若重合则去掉末点（环形周期插值不需要重复点）
        let (first, last) = (cleaned[0], cleaned[cleaned.len() - 1]);
        if ((first.0 - last.0).powi(2) + (first.1 - last.1).powi(2)).sqrt() < 0.5 {
            cleaned.pop();
        }
    }
    let m = cleaned.len();
    if m < 2 {
        return (Vec::new(), vec![0.0], (cx, cy));
    }

    // ③ 分段平滑采样
    let mut dense: Vec<(f64, f64)> = Vec::with_capacity(m * 8);
    for i in 0..m {
        let p0 = cleaned[(i + m - 1) % m];
        let p1 = cleaned[i];
        let p2 = cleaned[(i + 1) % m];
        let p3 = cleaned[(i + 2) % m];
        let seg = ((p2.0 - p1.0).powi(2) + (p2.1 - p1.1).powi(2)).sqrt();
        let samples = ((seg / 1.5).ceil() as usize).clamp(3, 96);
        for j in 0..samples {
            dense.push(catmull_rom(p0, p1, p2, p3, j as f64 / samples as f64));
        }
    }
    let arcs = arc_table(&dense);
    (dense, arcs, (cx, cy))
}

/// 点到折线的最近距离（米）与该点投影到的段序号，`closed` 决定是否算首尾连线。
pub fn distance_to_polyline(
    pt: (f64, f64),
    path: &[(f64, f64)],
    closed: bool,
) -> (f64, Option<usize>) {
    if path.len() < 2 {
        return (f64::INFINITY, None);
    }
    let mut best = (f64::INFINITY, None);
    let segs = if closed { path.len() } else { path.len() - 1 };
    for i in 0..segs {
        let a = path[i];
        let b = path[(i + 1) % path.len()];
        let (ax, ay) = ((a.1) * MET_PER_DEG_LNG, (a.0) * MET_PER_DEG_LAT);
        let (bx, by) = ((b.1) * MET_PER_DEG_LNG, (b.0) * MET_PER_DEG_LAT);
        let (px, py) = ((pt.1) * MET_PER_DEG_LNG, (pt.0) * MET_PER_DEG_LAT);
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = dx * dx + dy * dy;
        let t = if len2 <= 1e-9 {
            0.0
        } else {
            (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
        };
        let (qx, qy) = (ax + dx * t, ay + dy * t);
        let d = ((px - qx).powi(2) + (py - qy).powi(2)).sqrt();
        if d < best.0 {
            best = (d, Some(i));
        }
    }
    best
}

/// 折线长度（米）。
pub fn polyline_len_m(path: &[(f64, f64)], closed: bool) -> f64 {
    if path.len() < 2 {
        return 0.0;
    }
    let mut total = 0.0;
    let segs = if closed { path.len() } else { path.len() - 1 };
    for i in 0..segs {
        let a = path[i];
        let b = path[(i + 1) % path.len()];
        let dx = (b.1 - a.1) * MET_PER_DEG_LNG;
        let dy = (b.0 - a.0) * MET_PER_DEG_LAT;
        total += (dx * dx + dy * dy).sqrt();
    }
    total
}

/// 打卡点 BD 系 (lat, lng) → 闭合平面路径 + 弧长表 + 中心。
pub type PointRing = (Vec<(f64, f64)>, Vec<f64>, (f64, f64));
pub fn make_point_ring(bd_points: &[(f64, f64)]) -> PointRing {
    let n = bd_points.len();
    let cx = bd_points.iter().map(|q| q.0).sum::<f64>() / n as f64;
    let cy = bd_points.iter().map(|q| q.1).sum::<f64>() / n as f64;
    let mut ordered = bd_points.to_vec();
    ordered.sort_by(|a, b| {
        let ka = (a.0 - cx).atan2(a.1 - cy);
        let kb = (b.0 - cx).atan2(b.1 - cy);
        ka.partial_cmp(&kb).unwrap()
    });
    let plane: Vec<(f64, f64)> = ordered
        .iter()
        .map(|q| ((q.1 - cy) * MET_PER_DEG_LNG, (q.0 - cx) * MET_PER_DEG_LAT))
        .collect();
    let samples = 18usize;
    let mut dense = Vec::with_capacity(n * samples);
    for i in 0..n {
        let p0 = plane[(i + n - 1) % n];
        let p1 = plane[i];
        let p2 = plane[(i + 1) % n];
        let p3 = plane[(i + 2) % n];
        for j in 0..samples {
            dense.push(catmull_rom(p0, p1, p2, p3, j as f64 / samples as f64));
        }
    }
    let mut arcs = vec![0.0f64];
    for i in 1..=dense.len() {
        let a = dense[i - 1];
        let b = dense[i % dense.len()];
        arcs.push(arcs[i - 1] + ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt());
    }
    (dense, arcs, (cx, cy))
}

/// 环线弧长 → 坐标（线性插值）。
pub fn ring_point_at(dense: &[(f64, f64)], arcs: &[f64], s: f64) -> (f64, f64) {
    let total = *arcs.last().unwrap_or(&1.0);
    let s = s.rem_euclid(total);
    let mut lo = 0usize;
    let mut hi = arcs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if arcs[mid] < s {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let i = lo.max(1);
    let a = dense[(i - 1) % dense.len()];
    let b = dense[i % dense.len()];
    let seg = arcs[i] - arcs[i - 1];
    let t = if seg > 0.0 {
        (s - arcs[i - 1]) / seg
    } else {
        0.0
    };
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

pub fn to_bd(x: f64, y: f64, c_lat: f64, c_lng: f64) -> (f64, f64) {
    (c_lat + y / MET_PER_DEG_LAT, c_lng + x / MET_PER_DEG_LNG)
}

// ── 坐标基准转换：WGS84 ↔ GCJ-02 ↔ BD-09 ─────────────────────────
// 打卡点来自服务端为 BD-09；OSM 路网为 WGS84。真实道路路由需先把路网
// 逐节点转成 BD-09，与经典模式（直接在 BD-09 上拟合打卡点环）保持一致。

const X_PI: f64 = std::f64::consts::PI * 3000.0 / 180.0;
const WGS_A: f64 = 6378245.0;
const WGS_EE: f64 = 0.00669342162296594323;

fn out_of_china(lat: f64, lng: f64) -> bool {
    lng < 72.004 || lng > 137.8347 || lat < 0.8293 || lat > 55.8271
}

fn transform_lat(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut ret = -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    ret += (20.0 * (y * pi).sin() + 40.0 * (y / 3.0 * pi).sin()) * 2.0 / 3.0;
    ret += (160.0 * (y / 12.0 * pi).sin() + 320.0 * (y * pi / 30.0).sin()) * 2.0 / 3.0;
    ret
}

fn transform_lng(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut ret = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    ret += (20.0 * (x * pi).sin() + 40.0 * (x / 3.0 * pi).sin()) * 2.0 / 3.0;
    ret += (150.0 * (x / 12.0 * pi).sin() + 300.0 * (x / 30.0 * pi).sin()) * 2.0 / 3.0;
    ret
}

/// WGS84 → GCJ-02（火星坐标，境内偏移）。
pub fn wgs84_to_gcj02(lat: f64, lng: f64) -> (f64, f64) {
    if out_of_china(lat, lng) {
        return (lat, lng);
    }
    let pi = std::f64::consts::PI;
    let mut dlat = transform_lat(lng - 105.0, lat - 35.0);
    let mut dlng = transform_lng(lng - 105.0, lat - 35.0);
    let radlat = lat / 180.0 * pi;
    let mut magic = radlat.sin();
    magic = 1.0 - WGS_EE * magic * magic;
    let sqrtmagic = magic.sqrt();
    dlat = (dlat * 180.0) / ((WGS_A * (1.0 - WGS_EE)) / (magic * sqrtmagic) * pi);
    dlng = (dlng * 180.0) / (WGS_A / sqrtmagic * radlat.cos() * pi);
    (lat + dlat, lng + dlng)
}

/// GCJ-02 → BD-09（`wire::bd09_to_gcj02` 的逆变换）。
pub fn gcj02_to_bd09(gcj_lat: f64, gcj_lng: f64) -> (f64, f64) {
    let z = (gcj_lng * gcj_lng + gcj_lat * gcj_lat).sqrt() + 0.00002 * (gcj_lat * X_PI).sin();
    let theta = gcj_lat.atan2(gcj_lng) + 0.000003 * (gcj_lng * X_PI).cos();
    (z * theta.sin() + 0.006, z * theta.cos() + 0.0065)
}

/// WGS84 → BD-09（OSM 路网对齐打卡点用）。
pub fn wgs84_to_bd09(lat: f64, lng: f64) -> (f64, f64) {
    let (g_lat, g_lng) = wgs84_to_gcj02(lat, lng);
    gcj02_to_bd09(g_lat, g_lng)
}

/// GCJ-02 → WGS84（`wgs84_to_gcj02` 的数值逆，迭代到 1e-9 度以内）。
///
/// 正向变换无闭式逆，这里用不动点迭代：每轮用当前估计值正变换，
/// 与目标的残差回减。境内偏移量级为 1e-3 度，3~4 轮即收敛。
pub fn gcj02_to_wgs84(gcj_lat: f64, gcj_lng: f64) -> (f64, f64) {
    if out_of_china(gcj_lat, gcj_lng) {
        return (gcj_lat, gcj_lng);
    }
    let (mut lat, mut lng) = (gcj_lat, gcj_lng);
    for _ in 0..16 {
        let (t_lat, t_lng) = wgs84_to_gcj02(lat, lng);
        let (d_lat, d_lng) = (t_lat - gcj_lat, t_lng - gcj_lng);
        if d_lat.abs() < 1e-10 && d_lng.abs() < 1e-10 {
            break;
        }
        lat -= d_lat;
        lng -= d_lng;
    }
    (lat, lng)
}

/// BD-09 → WGS84（百度坐标直转 GPS 原始坐标，供用户对照手机地图）。
pub fn bd09_to_wgs84(bd_lat: f64, bd_lng: f64) -> (f64, f64) {
    let (g_lat, g_lng) = super::wire::bd09_to_gcj02(bd_lat, bd_lng);
    gcj02_to_wgs84(g_lat, g_lng)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三套基准互转：往返误差应在毫米级（1e-6 度 ≈ 0.1 m）。
    #[test]
    fn datum_roundtrips() {
        let bd = (38.901678, 121.540241);
        let (g_lat, g_lng) = super::super::wire::bd09_to_gcj02(bd.0, bd.1);
        let (w_lat, w_lng) = gcj02_to_wgs84(g_lat, g_lng);
        // GCJ → WGS 为正变换的数值逆
        let (back_lat, back_lng) = wgs84_to_gcj02(w_lat, w_lng);
        assert!(
            (back_lat - g_lat).abs() < 1e-9,
            "lat={back_lat} 期望 {g_lat}"
        );
        assert!(
            (back_lng - g_lng).abs() < 1e-9,
            "lng={back_lng} 期望 {g_lng}"
        );
        // BD → WGS → BD 闭合
        let (bd_lat, bd_lng) = wgs84_to_bd09(w_lat, w_lng);
        assert!((bd_lat - bd.0).abs() < 1e-6, "lat={bd_lat} 期望 {}", bd.0);
        assert!((bd_lng - bd.1).abs() < 1e-6, "lng={bd_lng} 期望 {}", bd.1);
        // BD → WGS 直转与分步一致
        let (d_lat, d_lng) = bd09_to_wgs84(bd.0, bd.1);
        assert!((d_lat - w_lat).abs() < 1e-12 && (d_lng - w_lng).abs() < 1e-12);
    }

    /// 境外坐标不做偏移（避免海外用户被强行平移）。
    #[test]
    fn outside_china_is_identity() {
        assert_eq!(gcj02_to_wgs84(48.8566, 2.3522), (48.8566, 2.3522));
    }
}

pub fn fmt_gain_time(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}
