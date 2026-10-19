//! 模拟位置页：自己设定本次跑步的定位锚点，并在可视化地图上直接画路线。
//!
//! 定位锚点决定「向服务端要哪个学校的打卡点」，与设备信息页的
//! `anchor_lat/anchor_lon` 语义相同但更可控：
//! - 支持 WGS84 / GCJ-02 / BD-09 三种基准，内部统一换算成协议用的 BD-09；
//! - 粘贴手机地图里复制的坐标（自动纠正「经度,纬度」顺序）；
//! - 在线底图（OSM 瓦片）+ 打卡点云，取点/画线都在同一块画布上；
//! - 手绘路线可撤销/拖拽顶点/闭环/一键串上打卡点，跑完按它生成轨迹；
//! - 命名收藏，一键切换。
//!
//! 保存后写进 config.json（`sim_location` / `sim_favorites` / `drawn_route`），
//! CLI 的 `location` 子命令读写同一份，两端互通。

use super::map::MapState;
use super::{mobile, theme, App};
use crate::location::{self, Datum, SimLocation};
use crate::platform::InputKind;
use crate::track::drawn::{DrawnRoute, CHECKPOINT_TOL_M, MIN_USABLE_M};
use eframe::egui;

/// 地图画布高度。
const MAP_HEIGHT: f32 = 240.0;
/// 画路线用的画布更高，方便画复杂线路。
const DRAW_HEIGHT: f32 = 420.0;
/// 校园范围在打卡点外接框外再留多少米。
const CAMPUS_MARGIN_M: f64 = 150.0;

/// 地图区在干什么。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MapMode {
    /// 单击地图 = 把模拟位置挪过去
    #[default]
    Pick,
    /// 单击地图 = 给手绘路线加一个拐点
    Draw,
}

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
    /// 地图模式：取点 / 画线。
    pub mode: MapMode,
    /// 手绘路线（BD-09 顶点）。
    pub route: DrawnRoute,
    /// 撤销栈。
    pub undo: Vec<DrawnRoute>,
    /// 画线视图（与取点视图分开，各自保持缩放）。
    pub route_map: MapState,
    pub route_map_ready: bool,
    /// 上次按校园范围适配时的画布尺寸（尺寸变化要重新适配，首帧布局往往还没稳定）。
    pub route_fit_size: Option<egui::Vec2>,
    pub pick_fit_size: Option<egui::Vec2>,
    /// 正在拖动的顶点下标。
    pub drag_vertex: Option<usize>,
    /// 画线区的临时提示。
    pub route_note: String,
    /// 在线底图。
    pub tiles: crate::ui::tiles::TileLayer,
    pub tiles_ready: bool,
    /// 校园范围 `[min_lat, min_lon, max_lat, max_lon]`（BD-09），地图只显示这一块。
    pub campus: Option<[f64; 4]>,
    /// 是否锁定在校园范围内（配置项，界面可切）。
    pub campus_lock: bool,
}

impl SimLocPage {
    pub fn from_config(cfg: &crate::api::model::Config) -> Self {
        let mut page = Self::default();
        if let Some(sim) = cfg.sim_location.as_ref() {
            page.load(sim);
        }
        page.route = cfg.drawn_route.clone().unwrap_or_default();
        page.tiles = crate::ui::tiles::TileLayer::new(
            cfg.tile_enabled,
            &cfg.tile_url,
            crate::location::Datum::parse(&cfg.tile_datum).unwrap_or(Datum::Wgs84),
        );
        // 老配置（高德 style=8 注记叠加图）迁移到标准路网图
        if let Some(url) = crate::ui::tiles::migrate_legacy_url(&page.tiles.url_template) {
            page.tiles.url_template = url;
        }
        page.campus = cfg.campus_bbox;
        page.campus_lock = cfg.campus_lock;
        page
    }

    /// 按已缓存打卡点重算校园范围（外扩 150 m），没有缓存时退回用模拟位置附近。
    pub fn recompute_campus(&mut self, margin_m: f64) -> bool {
        let mut pts = self.points.clone();
        if pts.is_empty() {
            if let Some(p) = self.bd() {
                pts.push(p);
            }
        }
        match crate::location::bbox_from_points(&pts, margin_m) {
            Some(bbox) => {
                self.campus = Some(bbox);
                true
            }
            None => false,
        }
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

    /// 记录一次可撤销的路线状态（上限 64 步）。
    pub fn push_undo(&mut self) {
        self.undo.push(self.route.clone());
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
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
            ui.label(egui::RichText::new("可视化地图").strong());
            ui.selectable_value(&mut page.mode, MapMode::Pick, "取点");
            ui.selectable_value(&mut page.mode, MapMode::Draw, "画路线");
            if ui.small_button("重新读取点位").clicked() {
                reload_points = true;
            }
            let mut tiles_on = page.tiles.enabled;
            if ui.checkbox(&mut tiles_on, "在线底图").changed() {
                page.tiles.enabled = tiles_on;
                page.tiles_ready = false;
                self.config.tile_enabled = tiles_on;
                let _ = crate::api::model::save_config(&self.config);
                if !tiles_on {
                    page.tiles.reset();
                }
            }
        });
        if page.tiles.enabled {
            let current = crate::ui::tiles::preset_index(&page.tiles.url_template);
            let label = current
                .map(|i| crate::ui::tiles::PRESETS[i].0)
                .unwrap_or("自定义 URL");
            let mut chosen = current;
            let mut changed = false;
            mobile::row(ui, |ui| {
                ui.label("底图源：");
                egui::ComboBox::from_id_salt("tile_preset")
                    .width(240.0)
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (i, (name, _, _)) in crate::ui::tiles::PRESETS.iter().enumerate() {
                            if ui.selectable_value(&mut chosen, Some(i), *name).clicked() {
                                changed = true;
                            }
                        }
                    });
                let (loaded, inflight, failed) = page.tiles.stats();
                ui.colored_label(
                    theme::text_dim(),
                    format!("已加载 {loaded} · 加载中 {inflight} · 失败 {failed}"),
                );
            });
            if crate::ui::tiles::is_jpeg_style(&page.tiles.url_template) {
                ui.colored_label(
                    theme::warn(),
                    "卫星底图是 JPEG，当前版本没有 JPEG 解码器 —— 请用「标准」底图",
                );
            }
            if let Some(note) = page.tiles.note.clone() {
                ui.colored_label(theme::warn(), format!("底图异常：{note}"));
            }
        // ── 校园范围（只显示学校这一块）──────────────────────────
        if page.campus.is_none() && !page.points.is_empty() {
            if page.recompute_campus(CAMPUS_MARGIN_M) {
                self.config.campus_bbox = page.campus;
                let _ = crate::api::model::save_config(&self.config);
            }
        }
        let mut recenter = false;
        let mut recompute = false;
        let mut set_to_campus = false;
        mobile::row(ui, |ui| {
            ui.label("校园范围：");
            let mut lock = page.campus_lock;
            if ui.checkbox(&mut lock, "只看校园").changed() {
                page.campus_lock = lock;
                self.config.campus_lock = lock;
                let _ = crate::api::model::save_config(&self.config);
                page.map_ready = false;
                page.route_map_ready = false;
                page.route_fit_size = None;
                page.pick_fit_size = None;
            }
            match page.campus {
                Some(b) => {
                    let (w, h) = crate::location::bbox_size_m(b);
                    ui.colored_label(theme::text_dim(), format!("{w:.0}×{h:.0} m"));
                }
                None => {
                    ui.colored_label(theme::text_dim(), "未设置");
                }
            }
            if ui.button("定位到校园").clicked() {
                recenter = true;
            }
            if ui.small_button("按打卡点重算").clicked() {
                recompute = true;
            }
            if ui.small_button("位置设到校园中心").clicked() {
                set_to_campus = true;
            }
        });
        if recenter {
            page.map_ready = false;
            page.route_map_ready = false;
        }
        if set_to_campus {
            match page.campus {
                Some(b) => {
                    let (lat, lon) = crate::location::bbox_center(b);
                    let (d_lat, d_lon) = location::from_bd09(lat, lon, page.datum);
                    page.set_point(d_lat, d_lon);
                    page.map_ready = false;
                    page.flash(
                        format!(
                            "已填入校园中心 {d_lat:.6},{d_lon:.6}（{}），记得「应用并保存」",
                            page.datum.label()
                        ),
                        false,
                    );
                }
                None => page.flash("还没有校园范围，先点「按打卡点重算」", true),
            }
        }
        if recompute {
            if page.recompute_campus(CAMPUS_MARGIN_M) {
                self.config.campus_bbox = page.campus;
                let _ = crate::api::model::save_config(&self.config);
                page.map_ready = false;
                page.route_map_ready = false;
                page.flash("已按打卡点重算校园范围", false);
            } else {
                page.flash("没有打卡点缓存，先点「重新读取点位」", true);
            }
        }
        if changed {
            if let Some(i) = chosen {
                    let (_, url, datum) = crate::ui::tiles::PRESETS[i];
                    page.tiles.url_template = url.to_string();
                    page.tiles.datum = datum;
                    page.tiles.reset();
                    self.config.tile_url = url.to_string();
                    self.config.tile_datum = datum.code().to_string();
                    let _ = crate::api::model::save_config(&self.config);
                    page.flash(
                        format!("底图已切换为「{}」", crate::ui::tiles::PRESETS[i].0),
                        false,
                    );
                }
            }
        }
        if reload_points {
            page.points_loaded = false;
            page.points = crate::api::model::load_points_cache_any()
                .map(|pts| crate::api::points::points_bd(&pts))
                .unwrap_or_default();
            page.points_loaded = true;
            page.map_ready = false;
            page.route_map_ready = false;
            // 显式点击：经典模式下也要拉一次，才能看到学校范围并把锚点对齐到点位中心
            self.refresh_points(true);
            page.flash("已重新读取点位缓存", false);
        }
        if page.points.is_empty() {
            ui.colored_label(
                theme::warn(),
                "还没有打卡点缓存：点上面的「重新读取点位」拉一次（需已登录），或直接填坐标",
            );
        }
        match page.mode {
            MapMode::Pick => {
                ui.colored_label(
                    theme::text_dim(),
                    format!(
                        "黄点为已缓存的打卡点（{} 个）。单击地图把模拟位置移到该处；拖拽平移、滚轮缩放。",
                        page.points.len()
                    ),
                );
                self.draw_pick_map(ui, page);
            }
            MapMode::Draw => {
                ui.colored_label(
                    theme::text_dim(),
                    "单击地图加拐点，按住顶点拖动可调整，右键点顶点删除；空白处拖拽平移、滚轮缩放。",
                );
                self.draw_route_map(ui, page);
                self.draw_route_toolbar(ui, page);
            }
        }

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

        let size_changed = page.pick_fit_size != Some(canvas.size());
        let need_fit =
            !page.map_ready || (page.campus_lock && page.campus.is_some() && size_changed);
        if need_fit {
            let mut bounds = page.points.clone();
            if let Some(p) = pin {
                bounds.push(p);
            }
            let locked = page
                .campus
                .filter(|_| page.campus_lock)
                .map(|b| page.map.fit_bounds(canvas, b))
                .unwrap_or(false);
            if locked {
                // 已按校园范围适配
            } else if bounds.len() >= 2 {
                page.map.fit(canvas, &bounds);
            } else {
                let (lat, lon) =
                    pin.unwrap_or((self.identity.anchor_lat, self.identity.anchor_lon));
                page.map.center = (lat, lon);
                page.map.scale = 0.8;
                page.map.offset = egui::Vec2::ZERO;
            }
            page.map_ready = true;
            page.pick_fit_size = Some(canvas.size());
        }

        // 底图先铺，矢量层盖在上面；锁定校园时只铺校园框内
        let clip = campus_screen_rect(page.campus, page.campus_lock, &page.map, canvas);
        page.tiles.draw(ui, &page.map, canvas, clip);

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
        if let (true, Some(b)) = (page.campus_lock, page.campus) {
            page.map.clamp_to_bounds(canvas, b);
        }
        draw_campus_outline(&painter, canvas, &page.map, page.campus, page.campus_lock);
    }

    /// 画线画布：在线底图 + 打卡点 + 手绘折线（加/拖/删顶点、缩放、比例尺）。
    fn draw_route_map(&mut self, ui: &mut egui::Ui, page: &mut SimLocPage) {
        let (response, painter) = ui.allocate_painter(
            egui::Vec2::new(ui.available_width().max(200.0), DRAW_HEIGHT),
            egui::Sense::click_and_drag(),
        );
        let canvas = response.rect;
        painter.rect_filled(
            canvas,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(250, 252, 251),
        );

        // 首次进入：视野对准打卡点与已画路线
        let size_changed = page.route_fit_size != Some(canvas.size());
        let need_fit = !page.route_map_ready
            || (page.campus_lock && page.campus.is_some() && size_changed);
        if need_fit {
            let mut bounds = page.points.clone();
            bounds.extend(page.route.points.iter().copied());
            let locked = page
                .campus
                .filter(|_| page.campus_lock)
                .map(|b| page.route_map.fit_bounds(canvas, b))
                .unwrap_or(false);
            if locked {
                // 已按校园范围适配
            } else if bounds.len() >= 2 {
                page.route_map.fit(canvas, &bounds);
            } else {
                let (lat, lon) = page
                    .bd()
                    .unwrap_or((self.identity.anchor_lat, self.identity.anchor_lon));
                page.route_map.center = (lat, lon);
                page.route_map.scale = 0.5;
                page.route_map.offset = egui::Vec2::ZERO;
            }
            page.route_map_ready = true;
            page.route_fit_size = Some(canvas.size());
        }

        // ① 底图
        let clip = campus_screen_rect(page.campus, page.campus_lock, &page.route_map, canvas);
        let painted = page.tiles.draw(ui, &page.route_map, canvas, clip);
        if page.tiles.enabled && !painted {
            let (loaded, inflight, _) = page.tiles.stats();
            let msg = if loaded == 0 && inflight > 0 {
                "底图加载中…"
            } else if let Some(note) = &page.tiles.note {
                &format!("底图不可用：{note}")
            } else {
                "底图加载中…"
            };
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(13.0),
                theme::text_dim(),
            );
        }
        let note = page.tiles.note.clone();
        if let Some(n) = note {
            painter.text(
                egui::pos2(canvas.left() + 8.0, canvas.bottom() - 20.0),
                egui::Align2::LEFT_BOTTOM,
                format!("底图：{n}"),
                egui::FontId::proportional(11.0),
                theme::warn(),
            );
        }

        // ② 打卡点
        let cp_color = egui::Color32::from_rgb(240, 180, 0);
        for &(lat, lon) in &page.points {
            page.route_map
                .draw_point(&painter, canvas, lat, lon, cp_color, 3.5);
        }

        // ③ 模拟位置锚点（空心十字，避免和路线混在一起）
        if let Some((lat, lon)) = page.bd() {
            let c = page.route_map.to_screen(canvas, lat, lon);
            let stroke = egui::Stroke::new(1.5_f32, theme::err());
            painter.circle_stroke(c, 6.0, stroke);
            painter.line_segment(
                [egui::pos2(c.x - 9.0, c.y), egui::pos2(c.x + 9.0, c.y)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x, c.y - 9.0), egui::pos2(c.x, c.y + 9.0)],
                stroke,
            );
        }

        // ④ 手绘折线
        let route_pts = page.route.points.clone();
        if !route_pts.is_empty() {
            let mut screen: Vec<egui::Pos2> = route_pts
                .iter()
                .map(|(la, lo)| page.route_map.to_screen(canvas, *la, *lo))
                .collect();
            if page.route.closed && screen.len() >= 2 {
                screen.push(screen[0]);
            }
            if screen.len() >= 2 {
                painter.add(egui::Shape::line(
                    screen.clone(),
                    egui::Stroke::new(2.5_f32, egui::Color32::from_rgb(26, 115, 232)),
                ));
            }
            // 顶点：首点绿、末点红、其它白底蓝边
            for (i, p) in screen.iter().enumerate() {
                let is_first = i == 0;
                let is_last = i + 1 == route_pts.len() && !page.route.closed;
                let fill = if is_first {
                    egui::Color32::from_rgb(22, 160, 90)
                } else if is_last {
                    egui::Color32::from_rgb(220, 38, 38)
                } else {
                    egui::Color32::WHITE
                };
                let radius = if Some(i) == page.drag_vertex { 6.0 } else { 4.5 };
                painter.circle_filled(*p, radius, fill);
                painter.circle_stroke(
                    *p,
                    radius,
                    egui::Stroke::new(1.2_f32, egui::Color32::from_rgb(26, 115, 232)),
                );
            }
        }

        // ⑤ 比例尺
        draw_scale_bar(&painter, canvas, &page.route_map);

        // ── 交互 ──────────────────────────────────────────────
        let hover_pos = ui.input(|i| i.pointer.hover_pos());
        let hover = hover_pos.map(|p| canvas.contains(p)).unwrap_or(false);
        let scroll = ui.input(|i| i.raw_scroll_delta.y);
        if scroll != 0.0 && hover {
            let anchor = hover_pos.unwrap_or(canvas.center());
            let before = page.route_map.from_screen(canvas, anchor);
            let factor = (scroll * 0.0015).exp();
            page.route_map.scale = (page.route_map.scale * factor).clamp(0.01, 60.0);
            let after = page.route_map.to_screen(canvas, before.0, before.1);
            page.route_map.offset += anchor - after;
            if let (true, Some(b)) = (page.campus_lock, page.campus) {
                page.route_map.clamp_to_bounds(canvas, b);
            }
        }

        let pointer = response.interact_pointer_pos();
        if response.drag_started() {
            page.drag_vertex = pointer
                .and_then(|p| nearest_vertex(&page.route_map, canvas, &page.route.points, p));
        }
        if response.dragged() {
            match (page.drag_vertex, pointer) {
                (Some(idx), Some(pos)) => {
                    let (lat, lon) = page.route_map.from_screen(canvas, pos);
                    page.route.move_vertex(idx, lat, lon);
                }
                (None, _) => {
                    page.route_map.offset += ui.input(|i| i.pointer.delta());
                }
                _ => {}
            }
        }
        if response.drag_stopped() {
            page.drag_vertex = None;
        }

        if let (true, Some(b)) = (page.campus_lock, page.campus) {
            page.route_map.clamp_to_bounds(canvas, b);
        }
        draw_campus_outline(
            &painter,
            canvas,
            &page.route_map,
            page.campus,
            page.campus_lock,
        );

        // 左键单击空白处 = 追加拐点
        if response.clicked() {
            if let Some(pos) = pointer {
                let (lat, lon) = page.route_map.from_screen(canvas, pos);
                if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                    page.push_undo();
                    page.route.points.push((lat, lon));
                    page.route_note = format!(
                        "已加第 {} 个拐点（BD-09 {lat:.6},{lon:.6}）",
                        page.route.points.len()
                    );
                }
            }
        }
        // 右键点顶点 = 删除
        if response.secondary_clicked() {
            if let Some(pos) = pointer {
                if let Some(idx) =
                    nearest_vertex(&page.route_map, canvas, &page.route.points, pos)
                {
                    page.push_undo();
                    page.route.remove_vertex(idx);
                    page.route_note = format!("已删除第 {} 个拐点", idx + 1);
                }
            }
        }

        // 左上角提示
        painter.text(
            egui::pos2(canvas.left() + 8.0, canvas.top() + 8.0),
            egui::Align2::LEFT_TOP,
            format!(
                "{} · 单击加点 / 拖顶点调整 / 右键删点 / 滚轮缩放",
                if page.route.points.is_empty() {
                    "空路线".to_string()
                } else {
                    page.route.summary()
                }
            ),
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(70, 84, 80),
        );
    }

    /// 画线工具栏：统计、编辑、保存。
    fn draw_route_toolbar(&mut self, ui: &mut egui::Ui, page: &mut SimLocPage) {
        ui.add_space(4.0);
        let gaps = page.route.checkpoint_gaps(&page.points);
        let missed = gaps.iter().filter(|g| **g > CHECKPOINT_TOL_M).count();
        if page.points.is_empty() {
            ui.colored_label(
                theme::text_dim(),
                "没有打卡点缓存时也能画，但「串上打卡点」要先拉一次点位。",
            );
        } else if missed > 0 {
            ui.colored_label(
                theme::warn(),
                format!(
                    "{}/{} 个打卡点离路线超过 {:.0} m，点「串上打卡点」自动补进路线",
                    missed,
                    page.points.len(),
                    CHECKPOINT_TOL_M
                ),
            );
        } else {
            ui.colored_label(
                theme::ok(),
                format!("{} 个打卡点都在路线上", page.points.len()),
            );
        }

        let mut undo = false;
        let mut clear = false;
        let mut attach = false;
        let mut fit = false;
        let mut set_dist = false;
        let mut save = false;
        let mut remove_saved = false;
        // 路线与当前模拟位置不在同一片区域时提醒（换了学校忘清路线）
        if !page.route.is_empty() {
            if let Some((lat, lon)) = page.bd() {
                let far = page.route.max_distance_to(lat, lon);
                if far > 3000.0 {
                    ui.colored_label(
                        theme::warn(),
                        format!(
                            "手绘路线离当前模拟位置约 {:.1} km，可能不是这所学校——换学校记得清空重画",
                            far / 1000.0
                        ),
                    );
                }
            }
        }
        if !page.points.is_empty() {
            if let Some(anchor) = crate::api::model::cached_points_anchor() {
                ui.colored_label(
                    theme::text_dim(),
                    format!(
                        "点位缓存来自锚点 ({:.6},{:.6})",
                        anchor.latitude, anchor.longitude
                    ),
                );
            }
        }
        mobile::row(ui, |ui| {
            if ui.small_button("撤销").clicked() {
                undo = true;
            }
            if ui.small_button("清空").clicked() {
                clear = true;
            }
            let mut closed = page.route.closed;
            if ui.checkbox(&mut closed, "闭环").changed() {
                page.route.closed = closed;
            }
            if ui.button("串上打卡点").clicked() {
                attach = true;
            }
            if ui.small_button("适配视野").clicked() {
                fit = true;
            }
            if ui.button("按绘制长度设距离").clicked() {
                set_dist = true;
            }
        });
        mobile::row(ui, |ui| {
            if ui.add(theme::primary_btn("保存路线")).clicked() {
                save = true;
            }
            if self.config.drawn_route.is_some() && ui.button("清除已保存路线").clicked() {
                remove_saved = true;
            }
            if self.config.drawn_route.is_some() {
                ui.colored_label(theme::ok(), "跑步将按已保存的手绘路线生成");
            }
        });
        if !page.route_note.is_empty() {
            ui.colored_label(theme::text_dim(), page.route_note.clone());
        }

        if undo {
            match page.undo.pop() {
                Some(prev) => {
                    page.route = prev;
                    page.route_note = "已撤销".into();
                }
                None => page.route_note = "没有可撤销的操作".into(),
            }
        }
        if clear {
            page.push_undo();
            page.route = DrawnRoute::default();
            page.route_note = "已清空手绘路线".into();
        }
        if attach {
            page.push_undo();
            let n = page.route.attach_checkpoints(&page.points, CHECKPOINT_TOL_M);
            page.route_note = if n == 0 {
                "路线已经串上全部打卡点".into()
            } else {
                format!("已把 {n} 个打卡点插进路线")
            };
        }
        if fit {
            page.route_map_ready = false;
        }
        if set_dist {
            let km = page.route.len_m() / 1000.0;
            if km < 0.2 {
                page.route_note = "路线太短，先画长一点再设距离".into();
            } else {
                self.run_page.dist_min = (km * 0.97) as f32;
                self.run_page.dist_max = (km * 1.03) as f32;
                self.run_page.regen_plan();
                page.route_note =
                    format!("已把跑步距离设为 {:.2}–{:.2} km", km * 0.97, km * 1.03);
            }
        }
        if save {
            match page.route.validate() {
                Err(e) => page.flash(e, true),
                Ok(()) if !page.route.is_usable() => page.flash(
                    format!("路线太短（不足 {MIN_USABLE_M:.0} m），先画一段再保存"),
                    true,
                ),
                Ok(()) => {
                    self.config.drawn_route = Some(page.route.clone());
                    match crate::api::model::save_config(&self.config) {
                        Ok(()) => {
                            page.flash("√ 手绘路线已保存，跑步会按它生成轨迹", false);
                            self.invalidate_run_preview();
                        }
                        Err(e) => page.flash(format!("保存失败：{e}"), true),
                    }
                }
            }
        }
        if remove_saved {
            self.config.drawn_route = None;
            match crate::api::model::save_config(&self.config) {
                Ok(()) => {
                    page.flash("已清除已保存的手绘路线", false);
                    self.invalidate_run_preview();
                }
                Err(e) => page.flash(format!("保存失败：{e}"), true),
            }
        }
    }
}

/// 锁定校园时，底图只铺在这个屏幕矩形里（校园框 ∩ 画布）。
fn campus_screen_rect(
    campus: Option<[f64; 4]>,
    locked: bool,
    map: &MapState,
    canvas: egui::Rect,
) -> Option<egui::Rect> {
    if !locked {
        return None;
    }
    let b = campus?;
    let p0 = map.to_screen(canvas, b[0], b[1]);
    let p1 = map.to_screen(canvas, b[2], b[3]);
    let r = egui::Rect::from_min_max(
        egui::pos2(p0.x.min(p1.x), p0.y.min(p1.y)),
        egui::pos2(p0.x.max(p1.x), p0.y.max(p1.y)),
    );
    Some(r.intersect(canvas))
}

/// 画出校园范围外框（锁定时常显，未锁定时淡显）。
fn draw_campus_outline(
    painter: &egui::Painter,
    canvas: egui::Rect,
    map: &MapState,
    campus: Option<[f64; 4]>,
    locked: bool,
) {
    let Some(b) = campus else { return };
    let p0 = map.to_screen(canvas, b[0], b[1]);
    let p1 = map.to_screen(canvas, b[2], b[3]);
    let rect = egui::Rect::from_min_max(
        egui::pos2(p0.x.min(p1.x), p0.y.min(p1.y)),
        egui::pos2(p0.x.max(p1.x), p0.y.max(p1.y)),
    );
    let color = if locked {
        egui::Color32::from_rgb(13, 148, 136)
    } else {
        egui::Color32::from_rgb(170, 185, 180)
    };
    painter.rect_stroke(rect, 0.0, egui::Stroke::new(1.5_f32, color));
    painter.text(
        egui::pos2(rect.left() + 4.0, rect.top() + 2.0),
        egui::Align2::LEFT_TOP,
        "校园范围",
        egui::FontId::proportional(10.0),
        color,
    );
}

/// 屏幕距离最近且在 12 px 内的顶点。
fn nearest_vertex(
    map: &MapState,
    canvas: egui::Rect,
    points: &[(f64, f64)],
    pos: egui::Pos2,
) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, (lat, lon)) in points.iter().enumerate() {
        let p = map.to_screen(canvas, *lat, *lon);
        let d = (p - pos).length();
        if d <= 12.0 && best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

/// 左下角比例尺（自动挑 1/2/5 × 10^n 米）。
fn draw_scale_bar(painter: &egui::Painter, canvas: egui::Rect, map: &MapState) {
    let target_m = 100.0_f64;
    let px = (target_m * map.scale as f64) as f32;
    let (label, width) = if px > 260.0 {
        ("500 m", 500.0 * map.scale)
    } else if px < 40.0 {
        ("20 m", 20.0 * map.scale)
    } else {
        ("100 m", px)
    };
    let y = canvas.bottom() - 12.0;
    let x0 = canvas.left() + 10.0;
    let stroke = egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(70, 84, 80));
    painter.line_segment([egui::pos2(x0, y), egui::pos2(x0 + width, y)], stroke);
    painter.line_segment([egui::pos2(x0, y - 4.0), egui::pos2(x0, y + 4.0)], stroke);
    painter.line_segment(
        [egui::pos2(x0 + width, y - 4.0), egui::pos2(x0 + width, y + 4.0)],
        stroke,
    );
    painter.text(
        egui::pos2(x0, y - 6.0),
        egui::Align2::LEFT_BOTTOM,
        label,
        egui::FontId::proportional(10.0),
        egui::Color32::from_rgb(70, 84, 80),
    );
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
