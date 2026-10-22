//! 手绘路线：用户在可视化地图上画出来的折线（BD-09 顶点）。
//!
//! 只负责「几何 + 编辑 + 校验」，不碰协议；跑步流程拿到它以后由
//! [`super::generator::build_on_path`] 套用同一套轨迹生成器。

use serde::{Deserialize, Serialize};

use super::geom::{distance_to_polyline, polyline_len_m};

/// 最短可用长度：太短的线生成不出有效轨迹（判定窗口 2'21"-10'00"/km）。
pub const MIN_USABLE_M: f64 = 100.0;
/// 打卡点「算经过」的默认容差（米）。
pub const CHECKPOINT_TOL_M: f64 = 25.0;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DrawnRoute {
    /// 顶点 `(纬度, 经度)`，BD-09。
    #[serde(default)]
    pub points: Vec<(f64, f64)>,
    /// 是否闭环：终点连回起点。
    #[serde(default)]
    pub closed: bool,
}

impl DrawnRoute {
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn is_usable(&self) -> bool {
        self.points.len() >= 2 && self.len_m() >= MIN_USABLE_M
    }

    /// 折线长度（米）；闭环时包含最后一条回到起点的边。
    pub fn len_m(&self) -> f64 {
        polyline_len_m(&self.points, self.closed)
    }

    /// 一行摘要，用于日志与界面。
    pub fn summary(&self) -> String {
        format!(
            "{} 点 / {:.0} m / {}",
            self.points.len(),
            self.len_m(),
            if self.closed { "闭环" } else { "折返" }
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.points.len() < 2 {
            return Err("手绘路线至少需要 2 个点".into());
        }
        for (i, (lat, lon)) in self.points.iter().enumerate() {
            if !lat.is_finite() || !(-90.0..=90.0).contains(lat) {
                return Err(format!("第 {} 个点纬度无效: {lat}", i + 1));
            }
            if !lon.is_finite() || !(-180.0..=180.0).contains(lon) {
                return Err(format!("第 {} 个点经度无效: {lon}", i + 1));
            }
        }
        Ok(())
    }

    /// 每个打卡点到折线的最近距离（米），用于判断「这条路线会不会漏打卡点」。
    pub fn checkpoint_gaps(&self, checkpoints: &[(f64, f64)]) -> Vec<f64> {
        checkpoints
            .iter()
            .map(|cp| distance_to_polyline(*cp, &self.points, self.closed).0)
            .collect()
    }

    /// 把不在路线上的打卡点插进最近的边，使路线「串起」所有打卡点。
    ///
    /// 返回插入的点数。空路线时用打卡点按质心角排序生成一条环线。
    pub fn attach_checkpoints(&mut self, checkpoints: &[(f64, f64)], tol_m: f64) -> usize {
        if checkpoints.is_empty() {
            return 0;
        }
        if self.points.is_empty() {
            self.points = ring_order(checkpoints);
            self.closed = true;
            return self.points.len();
        }
        // 按边序号倒序插入：先插后面的边，前面已记录的下标不受影响
        let mut inserts: Vec<(usize, (f64, f64))> = Vec::new();
        for cp in checkpoints {
            let (gap, seg) = distance_to_polyline(*cp, &self.points, self.closed);
            if gap <= tol_m {
                continue;
            }
            if let Some(seg) = seg {
                inserts.push((seg, *cp));
            }
        }
        inserts.sort_by(|a, b| b.0.cmp(&a.0));
        let inserted = inserts.len();
        for (seg, cp) in inserts {
            let at = (seg + 1).min(self.points.len());
            self.points.insert(at, cp);
        }
        inserted
    }

    /// 删除某个顶点（越界忽略）。
    pub fn remove_vertex(&mut self, index: usize) {
        if index < self.points.len() {
            self.points.remove(index);
        }
    }

    /// 把顶点移到新位置（拖拽编辑用）。
    pub fn move_vertex(&mut self, index: usize, lat: f64, lon: f64) {
        if let Some(p) = self.points.get_mut(index) {
            *p = (lat, lon);
        }
    }

    /// 判断整条路线是否离某个坐标太远（换了学校时提示清空）。
    pub fn max_distance_to(&self, lat: f64, lon: f64) -> f64 {
        self.points
            .iter()
            .map(|p| {
                let dy = (p.0 - lat) * super::geom::MET_PER_DEG_LAT;
                let dx = (p.1 - lon) * super::geom::met_per_deg_lng(lat);
                (dx * dx + dy * dy).sqrt()
            })
            .fold(0.0f64, f64::max)
    }
}

/// 打卡点按质心角排序成环（与服务端返回顺序无关，避免画出自交的线）。
fn ring_order(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let n = points.len() as f64;
    let cx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let cy = points.iter().map(|p| p.1).sum::<f64>() / n;
    let mut out = points.to_vec();
    out.sort_by(|a, b| {
        let ka = (a.0 - cx).atan2(a.1 - cy);
        let kb = (b.0 - cx).atan2(b.1 - cy);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个边长约 100 m 的方形（BD 系，纬度每 0.0009 度 ≈ 100 m）。
    fn square() -> Vec<(f64, f64)> {
        vec![
            (38.9000, 121.5400),
            (38.9000, 121.54116),
            (38.9009, 121.54116),
            (38.9009, 121.5400),
        ]
    }

    #[test]
    fn length_counts_closing_edge_only_when_closed() {
        let mut route = DrawnRoute {
            points: square(),
            closed: false,
        };
        let open = route.len_m();
        route.closed = true;
        let closed = route.len_m();
        assert!(open > 250.0 && open < 400.0, "open={open}");
        assert!(closed > open + 60.0, "closed={closed} open={open}");
    }

    #[test]
    fn usable_needs_two_points_and_enough_length() {
        assert!(!DrawnRoute::default().is_usable());
        let tiny = DrawnRoute {
            points: vec![(38.9, 121.54), (38.9002, 121.5402)],
            closed: false,
        };
        assert!(tiny.len_m() < MIN_USABLE_M);
        assert!(!tiny.is_usable());
        let ok = DrawnRoute {
            points: square(),
            closed: true,
        };
        assert!(ok.is_usable());
    }

    #[test]
    fn attach_checkpoints_puts_every_point_on_the_route() {
        let mut route = DrawnRoute {
            points: square(),
            closed: true,
        };
        // 两个打卡点分别落在左右两条边外侧 40 m
        let cps = vec![(38.90045, 121.5416), (38.90045, 121.53956)];
        let before = route.checkpoint_gaps(&cps);
        assert!(before.iter().all(|g| *g > 30.0), "{before:?}");
        let inserted = route.attach_checkpoints(&cps, CHECKPOINT_TOL_M);
        assert_eq!(inserted, 2);
        let after = route.checkpoint_gaps(&cps);
        assert!(after.iter().all(|g| *g < 1.0), "{after:?}");
    }

    #[test]
    fn attach_on_empty_route_builds_a_ring() {
        let mut route = DrawnRoute::default();
        let cps = square();
        let inserted = route.attach_checkpoints(&cps, CHECKPOINT_TOL_M);
        assert_eq!(inserted, cps.len());
        assert!(route.closed);
        assert!(route.is_usable());
        let gaps = route.checkpoint_gaps(&cps);
        assert!(gaps.iter().all(|g| *g < 1.0), "{gaps:?}");
    }

    #[test]
    fn vertex_edit_helpers_are_bounds_checked() {
        let mut route = DrawnRoute {
            points: square(),
            closed: true,
        };
        route.move_vertex(1, 39.0, 121.6);
        assert_eq!(route.points[1], (39.0, 121.6));
        route.move_vertex(99, 1.0, 1.0); // 越界不 panic
        route.remove_vertex(3);
        assert_eq!(route.points.len(), 3);
        route.remove_vertex(99);
        assert_eq!(route.points.len(), 3);
    }

    #[test]
    fn validate_rejects_bad_coordinates() {
        let bad = DrawnRoute {
            points: vec![(91.0, 121.0), (38.0, 121.0)],
            closed: true,
        };
        assert!(bad.validate().is_err());
        let ok = DrawnRoute {
            points: square(),
            closed: true,
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn max_distance_flags_a_route_from_another_campus() {
        let route = DrawnRoute {
            points: square(),
            closed: true,
        };
        assert!(route.max_distance_to(38.9004, 121.5406) < 100.0);
        assert!(route.max_distance_to(38.9100, 121.5406) > 900.0);
    }

    #[test]
    fn serde_roundtrip_keeps_points_and_flag() {
        let route = DrawnRoute {
            points: square(),
            closed: true,
        };
        let text = serde_json::to_string(&route).unwrap();
        let back: DrawnRoute = serde_json::from_str(&text).unwrap();
        assert_eq!(route, back);
        // 旧配置缺字段时按默认值
        let sparse: DrawnRoute = serde_json::from_str("{}").unwrap();
        assert!(sparse.points.is_empty());
        assert!(!sparse.closed);
    }
}
