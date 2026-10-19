//! 地图画布绘制原语：等距投影 + 拖拽平移 + 滚轮缩放。
//!
//! 供「路网」页与「跑步」页复用，纯离线绘制折线/多边形/点。

use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke, Vec2};

use crate::track::geom::{MET_PER_DEG_LAT, MET_PER_DEG_LNG};

/// 地图视图状态（世界坐标为度）。
pub struct MapState {
    /// 参考中心（lat, lon）。
    pub center: (f64, f64),
    /// 屏幕每米像素（缩放）。
    pub scale: f32,
    /// 平移偏移（像素）。
    pub offset: Vec2,
    /// 最近是否被拖拽过（供外部判断）。
    pub dragged: bool,
}

impl Default for MapState {
    fn default() -> Self {
        MapState {
            center: (0.0, 0.0),
            scale: 0.3,
            offset: Vec2::ZERO,
            dragged: false,
        }
    }
}

impl MapState {
    /// 依据一组点（lat, lon）自动适配视野。
    pub fn fit(&mut self, rect: Rect, pts: &[(f64, f64)]) {
        if pts.len() < 2 {
            return;
        }
        let (mut slat, mut slon) = (0.0f64, 0.0f64);
        for (la, lo) in pts {
            slat += *la;
            slon += *lo;
        }
        let n = pts.len() as f64;
        self.center = (slat / n, slon / n);
        let (mut minx, mut maxx, mut miny, mut maxy) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for (la, lo) in pts {
            let x = (lo - self.center.1) * MET_PER_DEG_LNG;
            let y = (la - self.center.0) * MET_PER_DEG_LAT;
            minx = minx.min(x);
            maxx = maxx.max(x);
            miny = miny.min(y);
            maxy = maxy.max(y);
        }
        let w = (maxx - minx).max(40.0);
        let h = (maxy - miny).max(40.0);
        let sx = rect.width() as f64 / w;
        let sy = rect.height() as f64 / h;
        self.scale = ((sx.min(sy)) * 0.92) as f32;
        self.offset = Vec2::ZERO;
    }

    /// 度坐标 → 屏幕坐标。
    pub fn to_screen(&self, rect: Rect, lat: f64, lon: f64) -> Pos2 {
        let x = (lon - self.center.1) * MET_PER_DEG_LNG * self.scale as f64;
        let y = -(lat - self.center.0) * MET_PER_DEG_LAT * self.scale as f64;
        let c = rect.center();
        Pos2::new(
            (c.x as f64 + x + self.offset.x as f64) as f32,
            (c.y as f64 + y + self.offset.y as f64) as f32,
        )
    }

    /// 屏幕坐标 → 度坐标（`to_screen` 的逆，地图点选取点用）。
    pub fn from_screen(&self, rect: Rect, pos: Pos2) -> (f64, f64) {
        let c = rect.center();
        let x = (pos.x as f64 - c.x as f64 - self.offset.x as f64) / self.scale.max(1e-6) as f64;
        let y = -(pos.y as f64 - c.y as f64 - self.offset.y as f64) / self.scale.max(1e-6) as f64;
        (
            self.center.0 + y / MET_PER_DEG_LAT,
            self.center.1 + x / MET_PER_DEG_LNG,
        )
    }

    /// 把视野对准一个外接矩形 `[min_lat, min_lon, max_lat, max_lon]`。
    pub fn fit_bounds(&mut self, rect: Rect, bbox: [f64; 4]) -> bool {
        let [min_lat, min_lon, max_lat, max_lon] = bbox;
        let span_lat = (max_lat - min_lat).max(1e-6);
        let span_lon = (max_lon - min_lon).max(1e-6);
        let scale_x = rect.width() as f64 / (span_lon * MET_PER_DEG_LNG);
        let scale_y = rect.height() as f64 / (span_lat * MET_PER_DEG_LAT);
        let scale = scale_x.min(scale_y) * 0.94;
        if !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        self.center = ((min_lat + max_lat) / 2.0, (min_lon + max_lon) / 2.0);
        self.scale = scale as f32;
        self.offset = Vec2::ZERO;
        true
    }

    /// 把视野限制在 `bbox` 内：缩放下限是「刚好装下 bbox」，平移按轴夹紧。
    ///
    /// 某个轴上视口比 bbox 大时（宽画布看方形校园），该轴居中不再允许平移，
    /// 另一轴仍可平移但不会跑出范围——这是常见地图的 maxBounds 行为。
    pub fn clamp_to_bounds(&mut self, rect: Rect, bbox: [f64; 4]) {
        let [min_lat, min_lon, max_lat, max_lon] = bbox;
        let span_lat = (max_lat - min_lat).max(1e-7);
        let span_lon = (max_lon - min_lon).max(1e-7);
        let fit_x = rect.width() as f64 / (span_lon * MET_PER_DEG_LNG);
        let fit_y = rect.height() as f64 / (span_lat * MET_PER_DEG_LAT);
        // 再缩就会看到 bbox 之外
        let min_scale = (fit_x.min(fit_y) as f32).max(1e-4);
        if self.scale < min_scale {
            self.scale = min_scale;
        }
        let scale = self.scale.max(1e-4) as f64;
        // 视口中心当前对应的世界坐标
        let lat = self.center.0 + self.offset.y as f64 / (MET_PER_DEG_LAT * scale);
        let lon = self.center.1 - self.offset.x as f64 / (MET_PER_DEG_LNG * scale);
        let half_w = (rect.width() as f64 / 2.0) / (MET_PER_DEG_LNG * scale);
        let half_h = (rect.height() as f64 / 2.0) / (MET_PER_DEG_LAT * scale);
        let lat = if half_h >= span_lat / 2.0 {
            (min_lat + max_lat) / 2.0
        } else {
            lat.clamp(min_lat + half_h, max_lat - half_h)
        };
        let lon = if half_w >= span_lon / 2.0 {
            (min_lon + max_lon) / 2.0
        } else {
            lon.clamp(min_lon + half_w, max_lon - half_w)
        };
        self.center = (lat, lon);
        self.offset = Vec2::ZERO;
    }

    /// 处理拖拽/缩放交互（需在分配好 rect 后调用）。
    pub fn interact(&mut self, ui: &egui::Ui, rect: Rect) {
        self.dragged = false;
        let (drag, scroll) = ui.input(|i| {
            let hover = i
                .pointer
                .interact_pos()
                .map(|p| rect.contains(p))
                .unwrap_or(false);
            let dragging = hover && i.pointer.primary_down() && i.pointer.is_decidedly_dragging();
            (dragging, i.raw_scroll_delta.y)
        });
        if drag {
            let d = ui.input(|i| i.pointer.delta());
            self.offset += d;
            self.dragged = true;
        }
        if scroll != 0.0 {
            let factor = (scroll * 0.0015).exp() as f32;
            self.scale = (self.scale * factor).clamp(0.002, 200.0);
        }
    }

    pub fn draw_polyline(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        pts: &[(f64, f64)],
        color: Color32,
        width: f32,
    ) {
        if pts.len() < 2 {
            return;
        }
        let ps: Vec<Pos2> = pts
            .iter()
            .map(|(la, lo)| self.to_screen(rect, *la, *lo))
            .collect();
        painter.add(egui::Shape::line(ps, Stroke::new(width, color)));
    }

    pub fn draw_polygon(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        pts: &[(f64, f64)],
        color: Color32,
        width: f32,
    ) {
        if pts.len() < 3 {
            return;
        }
        let mut ps: Vec<Pos2> = pts
            .iter()
            .map(|(la, lo)| self.to_screen(rect, *la, *lo))
            .collect();
        ps.push(ps[0]);
        painter.add(egui::Shape::closed_line(ps, Stroke::new(width, color)));
    }

    pub fn draw_point(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        lat: f64,
        lon: f64,
        color: Color32,
        radius: f32,
    ) {
        let c = self.to_screen(rect, lat, lon);
        painter.circle_filled(c, radius, color);
    }
}

/// 收集一组折线/环的所有点，用于 fit。
pub fn collect_bounds(edges: &[Vec<(f64, f64)>]) -> Vec<(f64, f64)> {
    let mut pts: Vec<(f64, f64)> = Vec::new();
    for e in edges.iter().take(400) {
        for p in e {
            pts.push(*p);
        }
    }
    pts
}

/// 地图图例：颜色方块 + 说明文字。
pub fn legend(ui: &mut egui::Ui, items: &[(&str, Color32)]) {
    ui.horizontal(|ui| {
        ui.label("图例：");
        for (label, color) in items {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::same(2.0), *color);
            ui.label(*label);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(1200.0, 400.0),
        )
    }

    /// 锁定后视野中心必须落在 bbox 内，且任一侧不会把 bbox 甩出画面。
    #[test]
    fn clamp_keeps_viewport_on_campus() {
        // 校园框：约 445 m × 373 m（纬度 111 km/度，经度 86.6 km/度）
        let bbox = [28.265_9, 113.062_2, 28.269_5, 113.067_2];
        let r = rect();
        let mut map = MapState {
            center: (28.3, 113.2),
            scale: 0.05,
            ..Default::default()
        };
        map.clamp_to_bounds(r, bbox);
        // 中心回到 bbox 内
        let (lat, lon) = map.from_screen(r, r.center());
        assert!(lat >= bbox[0] && lat <= bbox[2], "center lat 越界 {lat}");
        assert!(lon >= bbox[1] && lon <= bbox[3], "center lon 越界 {lon}");
        // bbox 至少在一个轴上完整可见（缩放下限挡住了「看到城外」）
        let a = map.to_screen(r, bbox[0], bbox[1]);
        let b = map.to_screen(r, bbox[2], bbox[3]);
        let w_ok = a.x.min(b.x) >= r.left() - 1.0 && a.x.max(b.x) <= r.right() + 1.0;
        let h_ok = a.y.min(b.y) >= r.top() - 1.0 && a.y.max(b.y) <= r.bottom() + 1.0;
        assert!(w_ok || h_ok, "bbox 在两个轴上都超出画面");
        assert!(map.scale >= 0.5, "缩放没有收紧: {}", map.scale);
    }

    /// 正向放大也不该逃离 bbox。
    #[test]
    fn clamp_survives_zoom_and_pan() {
        let bbox = [28.265_9, 113.062_2, 28.269_5, 113.067_2];
        let r = rect();
        let mut map = MapState::default();
        assert!(map.fit_bounds(r, bbox), "fit_bounds 应成功");
        for scale in [1.0_f32, 3.0, 12.0] {
            map.scale = scale;
            map.offset = Vec2::new(-500.0, 300.0); // 故意拖出去
            map.clamp_to_bounds(r, bbox);
            let (lat, lon) = map.from_screen(r, r.center());
            assert!(lat >= bbox[0] && lat <= bbox[2], "center lat 越界 {lat}");
            assert!(lon >= bbox[1] && lon <= bbox[3], "center lon 越界 {lon}");
        }
    }

    /// fit_bounds 之后整个 bbox 应该在画面里。
    #[test]
    fn fit_bounds_shows_the_whole_box() {
        let bbox = [28.265_9, 113.062_2, 28.269_5, 113.067_2];
        let r = rect();
        let mut map = MapState::default();
        assert!(map.fit_bounds(r, bbox));
        let a = map.to_screen(r, bbox[0], bbox[1]);
        let b = map.to_screen(r, bbox[2], bbox[3]);
        assert!(a.x.min(b.x) >= r.left() - 1.0, "左边越界");
        assert!(a.x.max(b.x) <= r.right() + 1.0, "右边越界");
        assert!(a.y.min(b.y) >= r.top() - 1.0, "上边越界");
        assert!(a.y.max(b.y) <= r.bottom() + 1.0, "下边越界");
    }
}
