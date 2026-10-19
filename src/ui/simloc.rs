//! 模拟位置页：自己设定本次跑步的定位锚点。
//!
//! 定位锚点决定「向服务端要哪个学校的打卡点」，与设备信息页的
//! `anchor_lat/anchor_lon` 语义相同但更可控：
//! - 支持 WGS84 / GCJ-02 / BD-09 三种基准，内部统一换算成协议用的 BD-09；
//! - 粘贴手机地图里复制的坐标（自动纠正「经度,纬度」顺序）；
//! - 在地图上点选、对齐已缓存打卡点的中心；
//! - 命名收藏，一键切换。
//!
//! 保存后写进 config.json（`sim_location` / `sim_favorites`），CLI 的
//! `location` 子命令读写同一份，两端互通。

use super::map::MapState;
use super::{mobile, theme, App};
use crate::location::{self, Datum, SimLocation};
use crate::platform::InputKind;
use eframe::egui;

/// 地图画布高度。
const MAP_HEIGHT: f32 = 240.0;

#[derive(Default)]
pub struct SimLocPage {
    /// 「启用模拟位置」开关（镜像 config 里的 enabled，保存时写入）。
    pub enabled: bool,
    pub datum: Datum,
    /// 纬度/经度为界面权威值（按 `datum` 解释），避免数值与文本两份状态互相打架。
    pub lat_txt: String,
    pub lon_txt: String,
    pub city: String,
    pub label: String,
    /// 粘贴识别输入框。
    pub paste: String,
    pub msg: String,
    pub msg_error: bool,
    pub saved_flash: f32,
    pub map: MapState,
    pub map_ready: bool,
    /// 已缓存的打卡点（BD-09），地图取点的参照物。
    pub points: Vec<(f64, f64)>,
    pub points_loaded: bool,
}

impl SimLocPage {
    pub fn from_config(cfg: &crate::api::model::Config) -> Self {
        let mut page = Self::default();
        if let Some(sim) = cfg.sim_location.as_ref() {
            page.load(sim);
        }
        page
    }

    /// 把一份已保存的模拟位置灌进界面缓冲区。
    pub fn load(&mut self, sim: &SimLocation) {
        self.enabled = sim.enabled;
        self.datum = sim.datum;
        self.lat_txt = format!("{:.6}", sim.lat);
        self.lon_txt = format!("{:.6}", sim.lon);
        self.city = sim.city.clone();
        self.label = sim.label.clone();
        self.map_ready = false;
    }

    /// 回填坐标（保持当前基准）。
    fn set_point(&mut self, lat: f64, lon: f64) {
        self.lat_txt = format!("{lat:.6}");
        self.lon_txt = format!("{lon:.6}");
    }

    /// 按指定基准解析输入框。
    pub fn current_in(&self, datum: Datum) -> Result<SimLocation, String> {
        let lat = self.lat_txt.trim();
        let lon = self.lon_txt.trim();
        if lat.is_empty() || lon.is_empty() {
            return Err("请填写纬度和经度（6 位小数足够）".into());
        }
        let lat = lat
            .parse::<f64>()
            .map_err(|_| format!("纬度不是数字：{lat}"))?;
        let lon = lon
            .parse::<f64>()
            .map_err(|_| format!("经度不是数字：{lon}"))?;
        let mut sim = SimLocation::new(lat, lon, datum)?;
        sim.enabled = self.enabled;
        sim.city = self.city.trim().to_string();
        sim.label = self.label.trim().to_string();
        sim.anchor()?;
        Ok(sim)
    }

    pub fn current(&self) -> Result<SimLocation, String> {
        self.current_in(self.datum)
    }

    /// 当前输入的 BD-09 坐标（地图绘制用）。
    pub fn bd(&self) -> Option<(f64, f64)> {
        self.current().ok().and_then(|sim| sim.bd09().ok())
    }

    fn flash(&mut self, text: impl Into<String>, error: bool) {
        self.msg = text.into();
        self.msg_error = error;
    }
}

impl App {
    pub fn draw_sim_location(&mut self, ui: &mut egui::Ui) {
        let mut page = std::mem::take(&mut self.sim_loc_page);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| self.draw_sim_location_inner(ui, &mut page));
        self.sim_loc_page = page;
    }

    fn draw_sim_location_inner(&mut self, ui: &mut egui::Ui, page: &mut SimLocPage) {
        if !page.points_loaded {
            page.points = crate::api::model::load_points_cache_any()
                .map(|pts| crate::api::points::points_bd(&pts))
                .unwrap_or_default();
            page.points_loaded = true;
        }
        let compact = mobile::compact_ui(ui);
        ui.add_space(6.0);
        ui.label(egui::RichText::new("模拟位置（自定义定位锚点）").strong());
        ui.colored_label(
            theme::text_dim(),
            "这个坐标决定「去哪个学校拉打卡点、在哪跑」。启用后它覆盖设备信息页锚点，",
        );
        ui.colored_label(
            theme::text_dim(),
            "跑步流程也不再按打卡点自动漂移（漂移会把你设的点改掉）。",
        );

        ui.add_space(6.0);
        ui.checkbox(
            &mut page.enabled,
            "启用模拟位置（覆盖设备信息页的定位锚点）",
        );

        // ── 坐标基准 ────────────────────────────────────────────
        ui.add_space(6.0);
        let previous_datum = page.datum;
        mobile::row(ui, |ui| {
            ui.label("坐标基准：");
            for datum in Datum::ALL {
                ui.radio_value(&mut page.datum, datum, datum.label());
            }
            ui.colored_label(theme::text_dim(), page.datum.hint());
        });
        if page.datum != previous_datum {
            // 换基准只换数值，物理位置不变
            match page.current_in(previous_datum) {
                Ok(sim) => match sim.reexpressed(page.datum) {
                    Ok(converted) => {
                        page.set_point(converted.lat, converted.lon);
                        page.flash(
                            format!(
                                "已按 {} 重新表示同一位置：{:.6}, {:.6}",
                                page.datum.label(),
                                converted.lat,
                                converted.lon
                            ),
                            false,
                        );
                    }
                    Err(e) => page.flash(e, true),
                },
                Err(e) => page.flash(e, true),
            }
        }
        ui.colored_label(
            theme::text_dim(),
            "手机地图里的「经纬度」多为 WGS84；高德/腾讯是国内加密的 GCJ-02；百度是 BD-09。",
        );
        ui.colored_label(
            theme::text_dim(),
            "选错基准会偏 300-600 米，一个打卡点都拉不到。",
        );

        // ── 数值输入 ────────────────────────────────────────────
        ui.add_space(6.0);
        if compact {
            ui.label("纬度：");
            mobile::text_edit(
                ui,
                "sim_lat",
                &mut page.lat_txt,
                InputKind::Decimal,
                ui.available_width(),
            );
            ui.label("经度：");
            mobile::text_edit(
                ui,
                "sim_lon",
                &mut page.lon_txt,
                InputKind::Decimal,
                ui.available_width(),
            );
            ui.label("城市（提交记录用，可空）：");
            mobile::text_edit(
                ui,
                "sim_city",
                &mut page.city,
                InputKind::Text,
                ui.available_width(),
            );
            ui.label("备注名（收藏用，可空）：");
            mobile::text_edit(
                ui,
                "sim_label",
                &mut page.label,
                InputKind::Text,
                ui.available_width(),
            );
        } else {
            egui::Grid::new("sim_coord_grid")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("纬度（-90 ~ 90）：");
                    mobile::text_edit(ui, "sim_lat", &mut page.lat_txt, InputKind::Decimal, 240.0);
                    ui.end_row();
                    ui.label("经度（-180 ~ 180）：");
                    mobile::text_edit(ui, "sim_lon", &mut page.lon_txt, InputKind::Decimal, 240.0);
                    ui.end_row();
                    ui.label("城市（提交记录用，可空）：");
                    mobile::text_edit(ui, "sim_city", &mut page.city, InputKind::Text, 160.0);
                    ui.end_row();
                    ui.label("备注名（收藏用，可空）：");
                    mobile::text_edit(ui, "sim_label", &mut page.label, InputKind::Text, 160.0);
                    ui.end_row();
                });
        }

        // ── 粘贴识别 ────────────────────────────────────────────
        mobile::row(ui, |ui| {
            ui.label("粘贴坐标：");
            mobile::text_edit(ui, "sim_paste", &mut page.paste, InputKind::Text, 260.0);
            if ui.button("识别").clicked() {
                match location::parse_pair(&page.paste) {
                    Ok((lat, lon)) => {
                        page.set_point(lat, lon);
                        page.flash(
                            format!(
                                "已识别 {lat:.6},{lon:.6}（{}）；「经度,纬度」顺序已自动纠正",
                                page.datum.label()
                            ),
                            false,
                        );
                    }
                    Err(e) => page.flash(e, true),
                }
            }
        });

        // ── 校验与三套基准换算 ─────────────────────────────────
        ui.add_space(4.0);
        match page.current() {
            Ok(sim) => {
                if !sim.city.trim().is_empty() && !sim.city.ends_with('市') {
                    ui.colored_label(
                        theme::warn(),
                        "城市建议填全称（如「大连市」），它会写进提交记录的 address",
                    );
                }
                for datum in Datum::ALL {
                    if let Ok((lat, lon)) = sim.point_in(datum) {
                        let mark = if datum == page.datum {
                            "（当前填写基准）"
                        } else {
                            ""
                        };
                        ui.colored_label(
                            theme::plain(),
                            format!("{}：{lat:.6}, {lon:.6}{mark}", datum.label()),
                        );
                    }
                }
                match sim.bd09() {
                    Ok((lat, lon)) => ui.colored_label(
                        theme::text_dim(),
                        format!("提交/请求实际使用 BD-09：{lat:.6}, {lon:.6}"),
                    ),
                    Err(e) => ui.colored_label(theme::err(), e),
                };
            }
            Err(e) => {
                ui.colored_label(theme::err(), e);
            }
        }

        // ── 地图取点 ────────────────────────────────────────────
        ui.add_space(8.0);
        let mut reload_points = false;
        mobile::row(ui, |ui| {
            ui.label(egui::RichText::new("地图取点").strong());
            if ui.small_button("重新读取点位").clicked() {
                reload_points = true;
            }
        });
        if reload_points {
            page.points_loaded = false;
            page.points = crate::api::model::load_points_cache_any()
                .map(|pts| crate::api::points::points_bd(&pts))
                .unwrap_or_default();
            page.points_loaded = true;
            page.map_ready = false;
            // 显式点击：经典模式下也要拉一次，才能看到学校范围并把锚点对齐到点位中心
            self.refresh_points(true);
            page.flash("已重新读取点位缓存", false);
        }
        if page.points.is_empty() {
            ui.colored_label(
                theme::warn(),
                "还没有打卡点缓存：点上面的「重新读取点位」拉一次（需已登录），或直接填坐标",
            );
        } else {
            let origin = crate::api::model::cached_points_anchor()
                .map(|a| format!("，来自锚点 ({:.6},{:.6})", a.latitude, a.longitude))
                .unwrap_or_default();
            ui.colored_label(
                theme::text_dim(),
                format!("黄点为已缓存的打卡点（{} 个{origin}）", page.points.len()),
            );
            ui.colored_label(
                theme::text_dim(),
                "单击地图把模拟位置移到该处；拖拽平移、滚轮缩放。",
            );
        }
        self.draw_pick_map(ui, page);

        // ── 操作 ────────────────────────────────────────────────
        ui.add_space(8.0);
        let mut reset = false;
        let mut fill_device = false;
        let mut align_points = false;
        let mut clear = false;
        mobile::row(ui, |ui| {
            if ui.add(theme::primary_btn("应用并保存")).clicked() {
                match page.current() {
                    Ok(mut sim) => {
                        sim.enabled = page.enabled;
                        self.config.sim_location = Some(sim);
                        match crate::api::model::save_config(&self.config) {
                            Ok(()) => {
                                page.flash("√ 已保存，下次跑步生效", false);
                                page.saved_flash = 2.0;
                                self.status = "√ 模拟位置已保存".into();
                                self.log.push("√  模拟位置已保存到 config.json");
                                self.invalidate_run_preview();
                                self.refresh_points(false);
                            }
                            Err(e) => page.flash(format!("保存失败：{e}"), true),
                        }
                    }
                    Err(e) => page.flash(e, true),
                }
            }
            if ui.button("撤销修改").clicked() {
                reset = true;
            }
            if ui.button("用设备锚点填充").clicked() {
                fill_device = true;
            }
            if ui.button("对齐打卡点中心").clicked() {
                align_points = true;
            }
            if ui.button("清除模拟位置").clicked() {
                clear = true;
            }
            if page.saved_flash > 0.0 {
                ui.colored_label(theme::ok(), "已保存");
            }
        });
        if reset {
            match self.config.sim_location.clone() {
                Some(sim) => {
                    page.load(&sim);
                    page.flash("已恢复为已保存的模拟位置", false);
                }
                None => {
                    page.enabled = false;
                    page.lat_txt.clear();
                    page.lon_txt.clear();
                    page.city.clear();
                    page.label.clear();
                    page.flash("已恢复：当前没有保存过模拟位置", false);
                }
            }
        }
        if fill_device {
            // 设备信息页的锚点语义就是 BD-09
            let (lat, lon) = location::from_bd09(
                self.identity.anchor_lat,
                self.identity.anchor_lon,
                page.datum,
            );
            page.set_point(lat, lon);
            if page.city.trim().is_empty() {
                page.city = self.identity.city.clone();
            }
            page.flash("已填入设备信息页的定位锚点（该点也是 BD-09）", false);
        }
        if align_points {
            if page.points.is_empty() {
                page.flash("没有打卡点缓存，无法对齐", true);
            } else {
                let n = page.points.len() as f64;
                let (slat, slon) = page
                    .points
                    .iter()
                    .fold((0.0, 0.0), |acc, p| (acc.0 + p.0, acc.1 + p.1));
                let (lat, lon) = location::from_bd09(slat / n, slon / n, page.datum);
                page.set_point(lat, lon);
                page.flash(
                    format!("已对齐到 {} 个打卡点的质心", page.points.len()),
                    false,
                );
            }
        }
        if clear {
            self.config.sim_location = None;
            match crate::api::model::save_config(&self.config) {
                Ok(()) => {
                    page.enabled = false;
                    page.lat_txt.clear();
                    page.lon_txt.clear();
                    page.label.clear();
                    page.flash(
                        "√ 已清除模拟位置（收藏保留），跑步回到设备信息页锚点",
                        false,
                    );
                    self.invalidate_run_preview();
                }
                Err(e) => page.flash(format!("保存失败：{e}"), true),
            }
        }

        if !page.msg.is_empty() {
            let color = if page.msg_error {
                theme::err()
            } else {
                theme::ok()
            };
            ui.colored_label(color, &page.msg);
        }

        // ── 当前生效状态 ────────────────────────────────────────
        ui.add_space(6.0);
        ui.separator();
        match self.config.active_sim() {
            Some(sim) => match sim.anchor() {
                Ok(anchor) => {
                    ui.colored_label(
                        theme::ok(),
                        format!(
                            "当前生效「{}」→ BD-09 ({:.6},{:.6}) · 城市 {}",
                            sim.short_name(),
                            anchor.latitude,
                            anchor.longitude,
                            sim.city_or(&self.identity.city)
                        ),
                    );
                }
                Err(e) => {
                    ui.colored_label(theme::err(), format!("已保存的模拟位置无效：{e}"));
                }
            },
            None => {
                ui.colored_label(
                    theme::warn(),
                    format!(
                        "当前生效：设备锚点 ({:.6},{:.6}) · 城市 {}",
                        self.identity.anchor_lat, self.identity.anchor_lon, self.identity.city
                    ),
                );
                if self.identity.has_unconfigured_default_location() {
                    ui.colored_label(
                        theme::warn(),
                        "仍是默认大连配置，直接跑会被拒绝（提交前必须先设位置）",
                    );
                }
            }
        }

        // ── 收藏 ────────────────────────────────────────────────
        ui.add_space(8.0);
        ui.label(egui::RichText::new("收藏").strong());
        let mut save_fav = false;
        mobile::row(ui, |ui| {
            if ui.button("把当前坐标存为收藏").clicked() {
                save_fav = true;
            }
            ui.colored_label(theme::text_dim(), "收藏名取上面的「备注名」；同名会覆盖。");
        });
        if save_fav {
            match page.current() {
                Ok(mut sim) => {
                    if sim.label.trim().is_empty() {
                        page.flash("请先填写备注名，再保存到收藏", true);
                    } else {
                        sim.enabled = true;
                        let name = sim.label.clone();
                        self.config.sim_favorites.retain(|f| f.label != name);
                        self.config.sim_favorites.push(sim);
                        match crate::api::model::save_config(&self.config) {
                            Ok(()) => page.flash(format!("√ 已收藏「{name}」"), false),
                            Err(e) => page.flash(format!("保存失败：{e}"), true),
                        }
                    }
                }
                Err(e) => page.flash(e, true),
            }
        }

        if self.config.sim_favorites.is_empty() {
            ui.colored_label(theme::text_dim(), "（还没有收藏）");
        } else {
            let mut apply: Option<usize> = None;
            let mut remove: Option<usize> = None;
            for (index, fav) in self.config.sim_favorites.iter().enumerate() {
                mobile::row(ui, |ui| {
                    ui.label(fav.summary());
                    if ui.small_button("应用").clicked() {
                        apply = Some(index);
                    }
                    if ui.small_button("删除").clicked() {
                        remove = Some(index);
                    }
                });
            }
            if let Some(index) = apply {
                if let Some(fav) = self.config.sim_favorites.get(index).cloned() {
                    page.load(&fav);
                    page.enabled = true;
                    self.config.sim_location = Some(SimLocation {
                        enabled: true,
                        ..fav
                    });
                    match crate::api::model::save_config(&self.config) {
                        Ok(()) => {
                            page.flash("√ 已应用收藏并保存", false);
                            self.invalidate_run_preview();
                            self.refresh_points(false);
                        }
                        Err(e) => page.flash(format!("保存失败：{e}"), true),
                    }
                }
            }
            if let Some(index) = remove {
                if index < self.config.sim_favorites.len() {
                    let removed = self.config.sim_favorites.remove(index);
                    match crate::api::model::save_config(&self.config) {
                        Ok(()) => page.flash(format!("已删除收藏「{}」", removed.label), false),
                        Err(e) => page.flash(format!("保存失败：{e}"), true),
                    }
                }
            }
        }

        if page.saved_flash > 0.0 {
            page.saved_flash -= ui.ctx().input(|i| i.stable_dt);
        }
    }

    /// 点位地图（点击取点）。
    fn draw_pick_map(&mut self, ui: &mut egui::Ui, page: &mut SimLocPage) {
        let pin = page.bd();
        let (response, painter) = ui.allocate_painter(
            egui::Vec2::new(ui.available_width().max(200.0), MAP_HEIGHT),
            egui::Sense::click_and_drag(),
        );
        let canvas = response.rect;
        painter.rect_filled(
            canvas,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(250, 252, 251),
        );

        if !page.map_ready {
            let mut bounds = page.points.clone();
            if let Some(p) = pin {
                bounds.push(p);
            }
            if bounds.len() >= 2 {
                page.map.fit(canvas, &bounds);
            } else {
                let (lat, lon) =
                    pin.unwrap_or((self.identity.anchor_lat, self.identity.anchor_lon));
                page.map.center = (lat, lon);
                page.map.scale = 0.8;
                page.map.offset = egui::Vec2::ZERO;
            }
            page.map_ready = true;
        }

        let checkpoint = egui::Color32::from_rgb(240, 180, 0);
        for &(lat, lon) in &page.points {
            page.map
                .draw_point(&painter, canvas, lat, lon, checkpoint, 3.0);
        }
        if let Some((lat, lon)) = pin {
            page.map
                .draw_point(&painter, canvas, lat, lon, theme::err(), 5.5);
            page.map
                .draw_point(&painter, canvas, lat, lon, egui::Color32::WHITE, 2.0);
        }

        // 单击取点（拖拽过则 clicked() 为 false，不影响平移）
        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                if canvas.contains(pos) {
                    let (bd_lat, bd_lon) = page.map.from_screen(canvas, pos);
                    if (-90.0..=90.0).contains(&bd_lat) && (-180.0..=180.0).contains(&bd_lon) {
                        let (lat, lon) = location::from_bd09(bd_lat, bd_lon, page.datum);
                        page.set_point(lat, lon);
                        page.flash(
                            format!("已取点：{lat:.6}, {lon:.6}（{}）", page.datum.label()),
                            false,
                        );
                    }
                }
            }
        }

        let hint = match pin {
            Some((lat, lon)) => {
                format!("当前点 BD-09 {lat:.6},{lon:.6} · 单击取点 / 拖拽平移 / 滚轮缩放")
            }
            None => "单击取点 / 拖拽平移 / 滚轮缩放".to_string(),
        };
        painter.text(
            egui::Pos2::new(canvas.left() + 8.0, canvas.top() + 8.0),
            egui::Align2::LEFT_TOP,
            hint,
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(88, 104, 99),
        );
        if page.points.is_empty() {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "（无打卡点缓存，可先填坐标再取点）",
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(150, 160, 156),
            );
        }
        page.map.interact(ui, canvas);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_with(lat: &str, lon: &str, datum: Datum) -> SimLocPage {
        SimLocPage {
            enabled: true,
            datum,
            lat_txt: lat.into(),
            lon_txt: lon.into(),
            ..Default::default()
        }
    }

    #[test]
    fn current_parses_texts_and_keeps_metadata() {
        let mut page = page_with("38.901678", "121.540241", Datum::Bd09);
        page.city = " 大连市 ".into();
        page.label = "主操场".into();
        let sim = page.current().expect("解析成功");
        assert!(sim.enabled);
        assert_eq!(sim.datum, Datum::Bd09);
        assert_eq!(sim.city, "大连市");
        assert_eq!(sim.label, "主操场");
        assert!((sim.lat - 38.901678).abs() < 1e-9);
    }

    #[test]
    fn current_rejects_blank_and_bad_text() {
        assert!(page_with("", "", Datum::Bd09).current().is_err());
        assert!(page_with("abc", "121.5", Datum::Bd09).current().is_err());
        assert!(page_with("91", "121.5", Datum::Bd09).current().is_err());
    }

    /// 地图取点后的回填：数值按当前基准写回，BD-09 保持一致。
    #[test]
    fn set_point_roundtrips_through_bd09() {
        let mut page = page_with("0", "0", Datum::Wgs84);
        let bd = (38.901678, 121.540241);
        let (w_lat, w_lon) = location::from_bd09(bd.0, bd.1, Datum::Wgs84);
        page.set_point(w_lat, w_lon);
        let back = page.bd().expect("BD-09");
        assert!((back.0 - bd.0).abs() < 1e-6, "lat={}", back.0);
        assert!((back.1 - bd.1).abs() < 1e-6, "lon={}", back.1);
    }

    #[test]
    fn load_prefills_texts_from_saved_location() {
        let sim = SimLocation {
            enabled: true,
            lat: 39.9,
            lon: 116.4,
            datum: Datum::Gcj02,
            city: "北京市".into(),
            label: "操场".into(),
        };
        let page = SimLocPage::from_config(&crate::api::model::Config {
            sim_location: Some(sim),
            ..Default::default()
        });
        assert!(page.enabled);
        assert_eq!(page.datum, Datum::Gcj02);
        assert_eq!(page.lat_txt, "39.900000");
        assert_eq!(page.city, "北京市");
    }
}
