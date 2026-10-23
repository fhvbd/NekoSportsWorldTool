//! 自然轨迹生成器。
//!
//! 画像驱动：路径拟合闭合环（Catmull-Rom 平滑）→ 弧长表；
//! 采样间隔主 5s（80%）；速度曲线 = ramp × 疲劳 × 三正弦 × 余弦凹陷 × 微噪；
//! 正常点按速度曲线分配位移并归一到精确总距离；异常点（-1）零位移/跳变；
//! 哨兵点（首 type∈{0,7}、索引1 type=5、末 type=6）；结尾断崖；点位吸附。
//!
//! 路径来源两种：[`build`] 用打卡点拟合环（经典算法），[`build_on_path`] 用用户
//! 在可视化地图上画的折线——两者共用 [`build_on_ring`] 这一套统计/速度/步频逻辑。
#![allow(non_snake_case)]

use super::geom::{
    fmt_gain_time, make_path_ring, make_point_ring, ring_point_at, round_to, to_bd, Rng,
    MET_PER_DEG_LAT, MET_PER_DEG_LNG,
};
use super::model::{GenPoint, Segment, TenWindow, Track};
use super::postfix::apply_post_fixes;

/// 有效配速窗口（判定规则 2'21"-10'00"/km ≈ 1.667-7.092 m/s），硬边界留余量。
pub const SPEED_FLOOR: f64 = 1.90;
pub const SPEED_CEIL: f64 = 6.30;

/// 等比缩放逐点速度至目标总距：越界点钳在窗口边界，剩余差量由未饱和点分摊（迭代收敛）。
/// 与整体等比缩放的区别：任何一点的瞬时配速都不会越出有效窗口。
fn fit_speeds(w: &mut [f64], dts: &[f64], target: f64) {
    for _ in 0..24 {
        let cur: f64 = w.iter().zip(dts).map(|(x, dt)| x * dt).sum();
        if (cur - target).abs() <= 1.0 {
            break;
        }
        let k = target / cur;
        for x in w.iter_mut() {
            *x = (*x * k).clamp(SPEED_FLOOR, SPEED_CEIL);
        }
    }
    for x in w.iter_mut() {
        *x = x.clamp(SPEED_FLOOR, SPEED_CEIL);
    }
}

/// 轨迹生成主入口。points_bd 为 BD 系打卡点。
pub fn build(
    dist: f64,
    dur: i64,
    seed: u64,
    center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
) -> Track {
    // 参数兜底：dur<=0 会让采样点数变 0，`(0..n-1)` 下溢 → capacity overflow 直接崩；
    // dist 非有限会把速度算成 NaN；点位为空时用给定中心合成小圆环
    let dist = if dist.is_finite() && dist > 10.0 { dist } else { 1_000.0 };
    let dur = dur.max(60);
    let pts: Vec<(f64, f64)> = points_bd
        .iter()
        .copied()
        .filter(|(la, lo)| la.is_finite() && lo.is_finite())
        .collect();
    let ring_src: Vec<(f64, f64)> = if pts.is_empty() { vec![center] } else { pts };
    let (dense, arcs, c) = make_point_ring(&ring_src);
    build_on_ring(&dense, &arcs, c, dist, dur, seed, start_ms, &ring_src)
}

/// 手绘路线入口：`path_bd` 为用户画的折线（BD-09），`closed` 决定终点是否连回起点。
///
/// 折线会先被平滑成闭合环（未闭环时按原路折返闭合），再走与经典算法
/// 完全相同的速度曲线、步频、10 秒窗与点位吸附，所以提交结构与普通跑步一致。
pub fn build_on_path(
    path_bd: &[(f64, f64)],
    closed: bool,
    dist: f64,
    dur: i64,
    seed: u64,
    start_ms: i64,
    points_bd: &[(f64, f64)],
) -> Track {
    let dist = if dist.is_finite() && dist > 10.0 { dist } else { 1_000.0 };
    let dur = dur.max(60);
    let path: Vec<(f64, f64)> = path_bd
        .iter()
        .copied()
        .filter(|(la, lo)| la.is_finite() && lo.is_finite())
        .collect();
    let (dense, arcs, center) = make_path_ring(&path, closed);
    // 退化路线（点重合/过短）兜底成打卡点环，避免空路径除零
    if dense.len() < 2 {
        let (dense, arcs, center) = make_point_ring(points_bd);
        return build_on_ring(&dense, &arcs, center, dist, dur, seed, start_ms, points_bd);
    }
    build_on_ring(&dense, &arcs, center, dist, dur, seed, start_ms, points_bd)
}

/// 环形路径上的通用生成逻辑（经典算法与手绘路线共用）。
fn build_on_ring(
    dense: &[(f64, f64)],
    arcs: &[f64],
    pc: (f64, f64),
    dist: f64,
    dur: i64,
    seed: u64,
    start_ms: i64,
    points_bd: &[(f64, f64)],
) -> Track {
    let mut rng = Rng::new(seed);
    let dur_f = dur as f64;
    let (c_lat, c_lng) = pc;
    let direction: f64 = rng.choice(&[1.0, -1.0]);
    let s0 = rng.uniform(0.0, *arcs.last().unwrap_or(&400.0));
    let phase_v = rng.uniform(0.0, std::f64::consts::TAU);
    let phase_l = rng.uniform(0.0, std::f64::consts::TAU);

    let mut times = Vec::new();
    let mut t = 0.0;
    while t < dur_f {
        times.push(t);
        t += if rng.random() < 0.95 {
            5.0
        } else {
            rng.choice(&[1.0, 2.0, 3.0, 4.0, 6.0, 7.0, 8.0])
        };
    }
    let n = times.len();

    let n_dips = rng.choice(&[1, 1, 2]);
    let mut dips = Vec::new();
    for _ in 0..n_dips {
        dips.push((
            rng.uniform(0.15, 0.75) * dur_f,
            rng.uniform(25.0, 55.0),
            rng.uniform(0.08, 0.16),
        ));
    }
    let dip_factor = |t: f64| -> f64 {
        let mut f = 1.0;
        for &(c, hw, d) in &dips {
            if (t - c).abs() < hw {
                f *= 1.0 - d * 0.5 * (1.0 + std::f64::consts::PI * (t - c) / hw).cos();
            }
        }
        f
    };
    let base = dist / dur_f;
    // ── 全局节奏：热身 → 中段（可能有一次加速）→ 尾段加速或掉速
    //    之前每公里配速几乎贴着平均值，看起来像匀速机；这里给 20~60 s/km 的快慢差。
    //   「正分段」（后半程更快）与「掉速」（后半程更慢）各一半概率，
    //   全程趋势 + 热身 + 中段一次鼓包，叠起来每公里能差 30~50 s。
    let negative_split = rng.random() < 0.5;
    let trend_amp = rng.uniform(0.08, 0.13);
    let warm_frac = rng.uniform(0.06, 0.14);
    let warm_amp = rng.uniform(0.04, 0.09);
    let surge_at = rng.uniform(0.35, 0.65);
    let surge_w = rng.uniform(0.08, 0.16);
    let surge_amp = rng.uniform(0.04, 0.08);
    let rhythm = |p: f64| -> f64 {
        // 全程趋势：正分段越跑越快，否则越跑越慢
        let trend = if negative_split {
            1.0 - trend_amp + 2.0 * trend_amp * p
        } else {
            1.0 + trend_amp - 2.0 * trend_amp * p
        };
        let warm = if p < warm_frac {
            1.0 - warm_amp * (1.0 - p / warm_frac)
        } else {
            1.0
        };
        let surge = 1.0 + surge_amp * (-((p - surge_at) / surge_w).powi(2)).exp();
        warm * trend * surge
    };
    // 每公里再叠一点独立起伏（±2~4.5%），避免每公里都贴着平均值
    let total_km = (dist / 1000.0).max(0.1);
    let n_km = total_km.ceil() as usize + 1;
    let km_sigma = rng.uniform(0.02, 0.045);
    let mut km_off: Vec<f64> = (0..n_km).map(|_| rng.gauss(0.0, km_sigma)).collect();
    // 重抽几次，保证公里之间至少有 4.5% 的差异（否则整段配速会贴在平均值上）
    for _ in 0..6 {
        let hi = km_off.iter().cloned().fold(f64::MIN, f64::max);
        let lo = km_off.iter().cloned().fold(f64::MAX, f64::min);
        if hi - lo >= 0.045 {
            break;
        }
        km_off = (0..n_km).map(|_| rng.gauss(0.0, km_sigma)).collect();
    }
    let km_mean = km_off.iter().sum::<f64>() / n_km as f64;
    for v in km_off.iter_mut() {
        *v -= km_mean;
    }
    let mut ou = 0.0f64;          // 随机游走分量（配速不是几个正弦拼出来的）
    let mut w = Vec::with_capacity(n);
    for &tt in &times {
        let p = (tt / dur_f).clamp(0.0, 1.0);
        let ramp = if tt < 8.0 {
            (0.85 + 0.15 * (tt / 1.0f64.max(8.0f64.min(dur_f / 20.0)))).min(1.0)
        } else {
            1.0
        } * if tt > dur_f - 8.0 {
            1.0 + 0.03 * (tt - (dur_f - 8.0)) / 8.0
        } else {
            1.0
        };
        let km_done = (tt / dur_f) * dist / 1000.0;
        let fatigue = if km_done <= 0.5 {
            1.01
        } else {
            (1.01 - 0.05 * (km_done - 0.5)).max(0.90)
        };
        let wave = (1.0 + 0.010 * (std::f64::consts::TAU * tt / 115.0 + phase_v).sin()
            + 0.035 * (std::f64::consts::TAU * tt / 47.0 + phase_v * 2.3).sin()
            + 0.015 * (std::f64::consts::TAU * tt / 19.0 + phase_v * 3.7).sin())
            * dip_factor(tt);
        let noise = 1.0 + rng.gauss(0.0, 0.008);
        let ki = ((p * total_km) as usize).min(n_km - 1);
        ou = 0.93 * ou + rng.gauss(0.0, 0.010);
        w.push(base * ramp * fatigue * wave * noise * rhythm(p) * (1.0 + km_off[ki] + ou));
    }
    let mut dts: Vec<f64> = (0..n - 1).map(|i| times[i + 1] - times[i]).collect();
    dts.push(1.0f64.max(dur_f - times[n - 1]));
    // 逐点速度全部约束在有效配速窗口内，并精确命中目标距离
    fit_speeds(&mut w, &dts, dist);
    let seg_dist: Vec<f64> = (0..n).map(|i| w[i] * dts[i]).collect();
    let speeds: Vec<f64> = w.clone();

    let tl_w = [((-1i64, 4i64), 54u32), ((-1, 1), 27), ((-1, 12), 13), ((-1, 5), 3), ((-1, 6), 2)];
    let mut kinds: Vec<(i64, i64)> = Vec::with_capacity(n);
    for _ in 0..n {
        let u = rng.random();
        // type=3 大概是「低精度/网络定位」，占比过高时 App 会把那些段画成灰色：
        // 真人在操场跑，绝大多数点都是正常 GPS，这里让 type=0 占 ~88%
        if u < 0.025 {
            kinds.push((3, 1));
        } else if u < 0.990 {
            kinds.push((0, 1));
        } else if u < 0.995 {
            // 异常漂移点：原来 3% 且常有 100~300 m 瞬移，App 画线时会断开成虚线；
            // 降到 0.8% 并把位移限制在十几米内，轨迹保持连续
            kinds.push(rng.weighted(&tl_w));
        } else {
            kinds.push((rng.choice(&[1, 1, 1, 1, 2, 2]), 1));
        }
    }
    for i in 1..kinds.len() {
        let prev_drift = (-1 == kinds[i - 1].0) || kinds[i - 1].0 == 5 || kinds[i - 1].0 == 6;
        if kinds[i].0 != -1 && prev_drift && rng.random() < 0.08 {
            kinds[i] = (if rng.random() < 0.75 { 7 } else { 8 }, 1);
        }
    }
    for i in 1..n {
        if kinds[i].0 == -1 && kinds[i - 1].0 == -1 {
            kinds[i] = (rng.choice(&[3, 0]), 1);
        }
    }
    let normal_idx: Vec<usize> =
        (0..n).filter(|&i| kinds[i].0 != -1 && i > 1).collect();
    let share: f64 = normal_idx.iter().map(|&i| seg_dist[i]).sum();
    let share = if share == 0.0 { 1.0 } else { share };
    let mut disp_of = vec![0.0f64; n];
    for &i in &normal_idx {
        disp_of[i] = seg_dist[i] * (dist / share);
    }

    let mut locs: Vec<GenPoint> = Vec::with_capacity(n);
    let mut s = s0;
    let mut t_acc = 0.0f64;
    let mut dist_acc = 0.0f64;
    let mut steps_acc = 0.0f64;
    let mut jx = 0.0f64;
    let mut jy = 0.0f64;
    let mut alt = 82.0 + rng.uniform(-1.0, 1.0);
    let n_est = 1.max((dur_f / 5.0) as i64);
    let alt_sigma = rng.uniform(3.8, 6.2) / (0.40 * n_est as f64);
    let mut ten_t = 0.0f64;
    let mut ten_d = 0.0f64;
    let mut ten_st = 0.0f64;
    let mut ten_speed: Vec<TenWindow> = Vec::new();
    let mut ten_steps: Vec<TenWindow> = Vec::new();

    for i in 0..n {
        let dt = dts[i];
        let (typ, lt) = kinds[i];
        t_acc += dt;
        let pos = |ss: f64| ring_point_at(dense, arcs, ss);
        let mut d_step = 0.0f64;
        let px;
        let py;
        let x;
        let y;
        let rad;
        let state;
        if typ != -1 {
            d_step = disp_of[i];
            s += direction * d_step;
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            jx = 0.72 * jx + rng.gauss(0.0, 0.75);
            jy = 0.72 * jy + rng.gauss(0.0, 0.75);
            px = bx + jx;
            py = by + jy;
            rad = round_to(
                if typ == 3 { rng.uniform(1.4, 5.1) } else { rng.uniform(1.4, 2.4) },
                2,
            );
            // state=3 是「定位质量差」，占比过高时 App 容易把相邻段画成灰色/断开；
            // 真人正常跑步大多数点是好定位，这里把 1/2 的比重调高。
            // state=1 是「定位正常」。之前 1/2/3 大致各占 1/3，App 里几乎一半的段
            // 被画成灰色虚线；真人操场跑基本都是 1，这里给 ~90%
            // 只在极少数点上给"弱定位"，避免 App 把段画成灰色虚线
            state = if typ == 0 {
                rng.weighted(&[(1, 990), (2, 5), (3, 5)])
            } else {
                rng.weighted(&[(1, 960), (2, 20), (3, 20)])
            };
        } else {
            match lt {
                4 => {
                    if rng.random() >= 0.68 {
                        d_step = rng.uniform(1.0, 8.0);
                    }
                }
                1 => {
                    if rng.random() >= 0.83 {
                        d_step = rng.uniform(0.5, 3.0);
                    }
                }
                12 => d_step = rng.uniform(1.0, 6.0),
                5 => d_step = rng.uniform(1.0, 6.0),
                _ => d_step = rng.uniform(2.0, 8.0),
            }
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            if d_step > 0.0 {
                let ang = rng.uniform(0.0, std::f64::consts::TAU);
                px = x + d_step * ang.sin();
                py = y + d_step * ang.cos();
            } else if let Some(last) = locs.last() {
                // 零位移：精确复制上一点坐标（BD→平面）
                py = (last.gLat - c_lat) * MET_PER_DEG_LAT;
                px = (last.gLng - c_lng) * MET_PER_DEG_LNG;
            } else {
                px = x + jx;
                py = y + jy;
            }
            rad = if lt == 4 {
                // 半径过大时 App 会把这些点当成无效定位跳过，同样会造成断线
                round_to(if rng.random() < 0.75 { rng.uniform(6.0, 25.0) } else { rng.uniform(25.0, 60.0) }, 2)
            } else if lt == 1 {
                round_to(if rng.random() < 0.75 { rng.uniform(1.6, 12.0) } else { rng.uniform(12.0, 95.0) }, 2)
            } else if lt == 12 {
                round_to(rng.uniform(30.0, 125.0), 2)
            } else if lt == 5 {
                round_to(rng.uniform(25.0, 300.0), 2)
            } else {
                550.0
            };
            state = rng.weighted(&[(1, 102), (2, 124), (3, 136)]);
        }
        if typ != -1 {
            dist_acc += d_step; // 异常点漂移不计入累计距离
        }
        let (lat, lng) = to_bd(px, py, c_lat, c_lng);
        alt += 0.04 * (82.0 - alt) + rng.gauss(0.0, alt_sigma);
        let v_now = dist / dur_f;
        let stride_target = 0.62 + 0.17 * v_now
            + 0.03 * (std::f64::consts::TAU * t_acc / 200.0 + phase_l).sin()
            + rng.gauss(0.0, 0.008);
        let v_cad = v_now * (1.0 + 0.03 * (std::f64::consts::TAU * t_acc / 70.0 + phase_v).sin());
        let cad = (v_cad / stride_target * 60.0).clamp(100.0, 200.0);
        steps_acc += cad / 60.0 * dt;
        let nxt = pos(s + direction * 2.0);
        let brg = ((nxt.0 - x).atan2(nxt.1 - y).to_degrees() + rng.gauss(0.0, 35.0)).rem_euclid(360.0);
        // 异常点（-1）：avgSpeed 为累计均值（真人与此一致，不为 0）；
        // GPS 瞬时速度多为低速，偶发 15-46 km/h 漂移尖峰
        let (avg_sp, gps_speed) = if typ == -1 {
            let avg = round_to(dist_acc / t_acc.max(1.0), 4);
            let gps = if rng.random() < 0.12 {
                rng.uniform(15.0, 46.0)
            } else {
                rng.uniform(0.5, 6.0)
            };
            (avg, round_to(gps, 4))
        } else {
            let avg = round_to(d_step / dt, 4);
            let kmh = avg * 3.6;
            let sigma = (kmh * 0.08).max(0.05);
            let gps = if rng.random() < 0.20 {
                0.0
            } else {
                round_to((kmh + rng.gauss(0.0, sigma)).max(0.0), 4)
            };
            (avg, gps)
        };

        ten_t += dt;
        ten_d += speeds[i] * dt;
        ten_st += cad / 60.0 * dt;
        while ten_t >= 10.0 {
            let k = 10.0 / ten_t; // 只取前 10s 的量，余量留给下一窗
            let (out_d, out_st) = (ten_d * k, ten_st * k);
            ten_speed.push(TenWindow { time: 10, value: round_to(out_d, 2) });
            ten_steps.push(TenWindow { time: 10, value: round_to(out_st, 0) });
            ten_t -= 10.0;
            ten_d -= out_d;
            ten_st -= out_st;
        }
        locs.push(GenPoint {
            id: i as i64 + 1,
            flag: start_ms,
            lat: -1.0,
            lng: -1.0,
            gLat: round_to(lat, 7),
            gLng: round_to(lng, 7),
            speed: round_to(gps_speed, 4),
            avgSpeed: avg_sp,
            radius: rad,
            accuracy: rad,
            ptype: typ,
            locType: lt,
            hasAltitude: true,
            totalTime: round_to(t_acc, 0) as i64,
            totalDis: round_to(dist_acc, 4),
            validDis: round_to(dist_acc, 4),
            validTime: round_to(t_acc, 0) as i64,
            steps: steps_acc as i64,
            stepDistance: 0.0,
            gainTime: fmt_gain_time(start_ms + (t_acc * 1000.0) as i64),
            gainTimeMs: start_ms + (t_acc * 1000.0) as i64,
            queueNum: 0,
            coorType: "gcj02".into(),
            bdA: round_to(alt, 2),
            bdD: round_to(brg, 2),
            bdS: round_to((avg_sp * rng.uniform(0.6, 0.95)).max(0.0), 3),
            bdG: rng.choice(&[1, 1, 1, -1]),
            count: rng.randint(20, 88),
            dtr: 0.0,
            state,
            locationId: String::new(),
        });
    }
    if ten_t > 1.0 {
        // 尾窗：限制放大倍数 ≤1.4
        let k = (10.0 / ten_t).min(1.4);
        ten_speed.push(TenWindow { time: 10, value: round_to(ten_d * k, 2) });
        ten_steps.push(TenWindow { time: 10, value: round_to(ten_st * k, 0) });
    }

    let mut segments: Vec<Segment> = Vec::new();
    let (mut seg_t, mut seg_d, mut seg_n) = (0.0f64, 0.0f64, 0i64);
    let mut seg_v: Vec<f64> = Vec::new();
    for i in 0..n {
        seg_t += dts[i];
        seg_d += seg_dist[i];
        seg_v.push(speeds[i]);
        seg_n += 1;
        if seg_t >= 60.0 || i == n - 1 {
            segments.push(Segment {
                totalTime: round_to(seg_t, 0) as i64,
                distance: round_to(seg_d, 0) as i64,
                startTime: round_to((times[i] - seg_t) * 1000.0, 0) as i64,
                endTime: round_to(times[i] * 1000.0, 0) as i64,
                avgSpeed: round_to(seg_v.iter().sum::<f64>() / seg_v.len() as f64, 3),
                avgStep: round_to(steps_acc / 1.0f64.max(t_acc) * 60.0, 0) as i64,
                state: 0,
            });
            seg_t = 0.0;
            seg_d = 0.0;
            seg_v.clear();
            seg_n = 0;
        }
    }
    let _ = seg_n;

    apply_post_fixes(&mut locs, &mut rng, start_ms);

    // 点位吸附：<40m 精确落位（一对一，且不碰哨兵/异常点）
    //
    // 原来每个打卡点各自找最近轨迹点：相距 <40m 的两个打卡点会抢同一个点，
    // 后一个把前一个覆盖掉（至少一个被判"未经过"）；硬拉 40m 还会在
    // 相邻点之间造出 45m 的"瞬移"，App 会画成断开。
    // 这里改成：命中的点独占，位移按 1 / 0.5 / 0.25 摊给相邻点，保持折线连续。
    let kx = super::geom::met_per_deg_lng(
        points_bd.iter().map(|p| p.0).sum::<f64>() / points_bd.len().max(1) as f64,
    );
    let mut used = vec![false; locs.len()];
    for pl in points_bd {
        let mut best_i = None;
        let mut best_d = 1e18f64;
        for (i, q) in locs.iter().enumerate() {
            if used[i] || i == 0 || matches!(q.ptype, -1 | 5 | 6) {
                continue;
            }
            let dd = ((q.gLat - pl.0) * MET_PER_DEG_LAT).powi(2)
                + ((q.gLng - pl.1) * kx).powi(2);
            if dd < best_d {
                best_d = dd;
                best_i = Some(i);
            }
        }
        if let Some(i) = best_i {
            if best_d < 40.0 * 40.0 {
                used[i] = true;
                let d_lat = pl.0 - locs[i].gLat;
                let d_lng = pl.1 - locs[i].gLng;
                locs[i].gLat = round_to(pl.0, 7);
                locs[i].gLng = round_to(pl.1, 7);
                for (k, w) in [(1usize, 0.5f64), (2usize, 0.25)] {
                    for j in [i.wrapping_sub(k), i + k] {
                        if j >= locs.len() || used[j] || matches!(locs[j].ptype, -1 | 5 | 6) {
                            continue;
                        }
                        locs[j].gLat = round_to(locs[j].gLat + d_lat * w, 7);
                        locs[j].gLng = round_to(locs[j].gLng + d_lng * w, 7);
                    }
                }
            }
        }
    }

    let total_dis = round_to(dist, 3);
    // 结尾断崖会给最后一个正常点追加 6~14s（postfix），结束时间必须跟着走：
    // 否则提交的 stopTime 与末点 gainTime 差几秒，10 秒尾窗还会把末点排除在外
    let loop_t = round_to(t_acc, 0) as i64;
    let total_t = locs
        .last()
        .map(|p| p.totalTime.max(loop_t))
        .unwrap_or(loop_t);
    Track {
        totalTime: total_t,
        totalDistance: total_dis,
        validDistance: total_dis,
        validTime: total_t,
        startTime: start_ms,
        startLatitude: locs[0].gLat,
        startLongitude: locs[0].gLng,
        totalSteps: steps_acc as i64,
        locations: locs,
        speedPerTenSec: ten_speed,
        stepsPerTenSec: ten_steps,
        segments,
    }
}
