//! 在线底图（XYZ 瓦片）：拉取 → 解码 → 纹理缓存 → 按当前地图视图绘制。
//!
//! 用途：让「模拟位置」页有一张真正的地图当画布，用户可以在上面直接画路线。
//! - 默认底图是 OSM 标准瓦片（PNG，WGS84）；可在 config 里改 `tile_url` / `tile_datum`，
//!   例如换成 GCJ-02 的高德瓦片（坐标系选 gcj02，注意遵守对方条款）。
//! - 只在视野内按需请求，单次最多 6 个并发，失败不反复重试，整段会话最多 400 张。
//! - 线程负责下载，UI 线程负责 PNG 解码与 `load_texture`（egui 要求）。

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui::{self, Color32, ColorImage, Rect, TextureHandle, TextureOptions};

use super::map::MapState;
use crate::location::{self, Datum};

/// OSM 底图每张瓦片的像素尺寸（标准 256）。
const TILE_PX: f64 = 256.0;
/// 并发上限。
const MAX_INFLIGHT: usize = 6;
/// 单帧最多铺多少张（避免一次性请求整个大范围）。
const MAX_TILES_PER_VIEW: usize = 64;
/// 整段会话最多请求多少张（对公共服务保持礼貌）。
const MAX_TILES_TOTAL: usize = 400;
/// 底图请求 UA（OSM 政策要求能识别客户端）。
const TILE_UA: &str = "NekoSportsWorldTool/0.3.0 (+https://github.com/fhvbd/NekoSportsWorldTool)";

/// 内置底图源：`(显示名, URL 模板, 坐标系)`。
///
/// 只支持 PNG（本机无 JPEG 解码器）。国内网络访问 OSM 官方常被 DNS 污染，
/// 所以默认用 CDN 上的 Carto（OSM 数据、带中文注记、许可要求署名）。
pub const PRESETS: [(&str, &str, Datum); 4] = [
    (
        "高德标准（中文路网，GCJ-02）",
        "https://webrd0{1-4}.is.autonavi.com/appmaptile?lang=zh_cn&size=1&scale=1&style=7&x={x}&y={y}&z={z}",
        Datum::Gcj02,
    ),
    (
        "Carto Voyager（PNG，英文注记）",
        "https://basemaps.cartocdn.com/rastertiles/voyager/{z}/{x}/{y}.png",
        Datum::Wgs84,
    ),
    (
        "OSM 德国镜像（PNG）",
        "https://tile.openstreetmap.de/{z}/{x}/{y}.png",
        Datum::Wgs84,
    ),
    (
        "OSM 官方（国内常被墙）",
        "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
        Datum::Wgs84,
    ),
];

/// 高德老配置用的是 `style=8`（只有路网注记的透明叠加图，铺在浅底上显得发白），
/// 这里迁移到 `style=7` 的标准路网图。
pub fn migrate_legacy_url(url: &str) -> Option<String> {
    if url.contains("appmaptile") && url.contains("style=8") && url.contains("wprd0") {
        return Some(url.replace("style=8", "style=7").replace("wprd0{1-4}", "webrd0{1-4}"));
    }
    None
}

/// 卫星底图（高德 `webst` / `style=6`）返回 JPEG，当前版本没有 JPEG 解码器。
pub fn is_jpeg_style(url: &str) -> bool {
    url.contains("webst") || url.contains("style=6") || url.contains("World_Imagery")
}

/// 当前模板对应哪个预设（用于下拉框回显）。
pub fn preset_index(url: &str) -> Option<usize> {
    PRESETS.iter().position(|(_, u, _)| *u == url)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Key {
    z: u8,
    x: u32,
    y: u32,
}

enum Tile {
    Ready(TextureHandle),
}

type Msg = (Key, Result<Vec<u8>, String>);

pub struct TileLayer {
    pub enabled: bool,
    pub url_template: String,
    pub datum: Datum,
    /// 当前使用的缩放级别（按地图比例自动选）。
    pub zoom: u8,
    /// 最近一次失败原因（界面提示用）。
    pub note: Option<String>,
    tiles: HashMap<Key, Tile>,
    inflight: HashSet<Key>,
    failed: HashSet<Key>,
    requested: usize,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for TileLayer {
    fn default() -> Self {
        // 默认不联网：真正启用由 config.tile_enabled 决定（见 SimLocPage::from_config）
        Self::new(
            false,
            "https://basemaps.cartocdn.com/rastertiles/voyager/{z}/{x}/{y}.png",
            Datum::Wgs84,
        )
    }
}

impl TileLayer {
    pub fn new(enabled: bool, url_template: &str, datum: Datum) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            enabled,
            url_template: url_template.to_string(),
            datum,
            zoom: 16,
            note: None,
            tiles: HashMap::new(),
            inflight: HashSet::new(),
            failed: HashSet::new(),
            requested: 0,
            tx,
            rx,
        }
    }

    /// 清空缓存（换底图源时用）。
    pub fn reset(&mut self) {
        self.tiles.clear();
        self.inflight.clear();
        self.failed.clear();
        self.requested = 0;
        self.note = None;
    }

    /// 在给定画布上铺底图，返回是否有任何瓦片已绘制。
    ///
    /// `clip` 用于「只看校园」模式：底图被裁剪到校园范围，范围外不铺图。
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        map: &MapState,
        rect: Rect,
        clip: Option<Rect>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        self.drain(ui.ctx());
        let visible = self.visible(map, rect);
        if visible.is_empty() {
            self.note.get_or_insert_with(|| "视野超出可用瓦片范围".into());
            return false;
        }
        self.zoom = visible[0].0.z;
        let painter = ui.painter_at(rect);
        let painter = match clip {
            Some(c) if c.width() > 1.0 && c.height() > 1.0 => painter.with_clip_rect(c),
            _ => painter,
        };
        let mut painted = false;
        for (key, tile_rect) in visible {
            match self.tiles.get(&key) {
                Some(Tile::Ready(tex)) => {
                    painter.image(
                        tex.id(),
                        tile_rect,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    painted = true;
                }
                None => {
                    painter.rect_filled(tile_rect, 0.0, Color32::from_rgb(226, 231, 228));
                    self.request(key);
                }
            }
        }
        if painted {
            painter.text(
                egui::pos2(rect.right() - 6.0, rect.bottom() - 4.0),
                egui::Align2::RIGHT_BOTTOM,
                if self.url_template.contains("openstreetmap") {
                    "© OpenStreetMap contributors"
                } else {
                    "在线底图"
                },
                egui::FontId::proportional(10.0),
                Color32::from_rgb(120, 130, 126),
            );
        }
        painted
    }

    /// `(已加载, 请求中, 失败)`，界面用来提示底图状态。
    pub fn stats(&self) -> (usize, usize, usize) {
        (self.tiles.len(), self.inflight.len(), self.failed.len())
    }

    /// 本帧需要绘制/请求的瓦片：`(key, 屏幕矩形)`，按离视野中心由近到远排序。
    pub fn visible(&self, map: &MapState, rect: Rect) -> Vec<(Key, Rect)> {
        let lat = map.center.0;
        if !lat.is_finite() || rect.width() <= 0.0 || rect.height() <= 0.0 {
            return Vec::new();
        }
        let scale = map.scale.max(0.02) as f64;
        let zf = (156_543.033_92 * lat.to_radians().cos() * scale)
            .log2()
            .round()
            .clamp(2.0, 19.0);
        let z = zf as u8;
        let n = 2f64.powi(z as i32);
        let (fx0, fy0) = {
            // 地图内部是 BD-09，而瓦片索引要按**瓦片自己的坐标系**算，
            // 否则 WGS84/GCJ02 的 500 m 偏移会让请求的瓦片整体错位（表现就是底图空白）
            let (t_lat, t_lon) = location::from_bd09(lat, map.center.1, self.datum);
            lonlat_to_tile(t_lon, t_lat, z, n)
        };
        // 屏幕上一张瓦片占多少像素：地面 256*r 米 × scale(px/m)
        let r = 156_543.033_92 * lat.to_radians().cos() / n;
        let tile_px = (TILE_PX * r * scale).max(24.0);
        let half_w = rect.width() as f64 / (2.0 * tile_px);
        let half_h = rect.height() as f64 / (2.0 * tile_px);
        // 注意：lonlat_to_tile 返回的是 0..1 的**归一化**瓦片坐标，必须乘 n 才是瓦片序号
        let cx_t = fx0 * n - map.offset.x as f64 / tile_px;
        let cy_t = fy0 * n - map.offset.y as f64 / tile_px;
        let x0 = (cx_t - half_w).floor() as i64;
        let x1 = (cx_t + half_w).ceil() as i64;
        let y0 = (cy_t - half_h).floor() as i64;
        let y1 = (cy_t + half_h).ceil() as i64;
        let mut out: Vec<(Key, Rect)> = Vec::new();
        for y in y0..=y1 {
            if y < 0 || y as f64 >= n {
                continue;
            }
            for x in x0..=x1 {
                let xw = x.rem_euclid(n as i64) as u32;
                let key = Key {
                    z,
                    x: xw,
                    y: y as u32,
                };
                let Some((lon0, lat0, lon1, lat1)) = tile_bounds(key, n) else {
                    continue;
                };
                let (bd_lat0, bd_lon0) = location::to_bd09(lat0, lon0, self.datum);
                let (bd_lat1, bd_lon1) = location::to_bd09(lat1, lon1, self.datum);
                let p0 = map.to_screen(rect, bd_lat0, bd_lon0);
                let p1 = map.to_screen(rect, bd_lat1, bd_lon1);
                let tr = Rect::from_min_max(
                    egui::pos2(p0.x.min(p1.x), p0.y.min(p1.y)),
                    egui::pos2(p0.x.max(p1.x), p0.y.max(p1.y)),
                );
                if tr.intersects(rect) {
                    out.push((key, tr));
                }
                if out.len() >= MAX_TILES_PER_VIEW {
                    break;
                }
            }
            if out.len() >= MAX_TILES_PER_VIEW {
                break;
            }
        }
        // 近中心优先，先看到地图
        let c = rect.center();
        out.sort_by(|a, b| {
            let da = a.1.center().distance(c);
            let db = b.1.center().distance(c);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    fn request(&mut self, key: Key) {
        if self.inflight.contains(&key) || self.failed.contains(&key) {
            return;
        }
        if self.inflight.len() >= MAX_INFLIGHT || self.requested >= MAX_TILES_TOTAL {
            return;
        }
        // 支持 {1-4} 这类子域占位（高德瓦片），按 tile 坐标轮转取一个，避免撞同一个域名
        let sub = (key.x + key.y) % 4 + 1;
        let url = self
            .url_template
            .replace("{1-4}", &sub.to_string())
            .replace("{z}", &key.z.to_string())
            .replace("{x}", &key.x.to_string())
            .replace("{y}", &key.y.to_string());
        self.inflight.insert(key);
        self.requested += 1;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let agent = crate::api::client::make_agent();
            let mut body = Vec::new();
            let res = match agent
                .get(&url)
                .set("User-Agent", TILE_UA)
                .call()
            {
                Ok(resp) => match resp.into_reader().read_to_end(&mut body) {
                    Ok(_) => Ok(body),
                    Err(e) => Err(format!("读取瓦片失败: {e}")),
                },
                Err(e) => Err(format!("{e}")),
            };
            tx.send((key, res)).ok();
        });
    }

    /// 收线程回包：解码 + 上传纹理。
    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok((key, res)) = self.rx.try_recv() {
            self.inflight.remove(&key);
            match res {
                Ok(bytes) => match image::load_from_memory(&bytes) {
                    Ok(img) => {
                        let rgba = img.to_rgba8();
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        let pixels = rgba.into_raw();
                        let color = ColorImage::from_rgba_unmultiplied(size, &pixels);
                        let tex = ctx.load_texture(
                            format!("tile-{}-{}-{}", key.z, key.x, key.y),
                            color,
                            TextureOptions::LINEAR,
                        );
                        self.tiles.insert(key, Tile::Ready(tex));
                    }
                    Err(e) => {
                        self.failed.insert(key);
                        self.note.get_or_insert_with(|| {
                            format!("底图解码失败（当前只支持 PNG 瓦片）: {e}")
                        });
                    }
                },
                Err(e) => {
                    self.failed.insert(key);
                    self.note.get_or_insert(e);
                }
            }
        }
        // 缓存上限：超过就整体清空（纹理句柄随 HashMap 释放）
        if self.tiles.len() > MAX_TILES_TOTAL * 2 {
            self.tiles.clear();
        }
    }
}

/// 经纬度 → 小数瓦片坐标。
fn lonlat_to_tile(lon: f64, lat: f64, _z: u8, _n: f64) -> (f64, f64) {
    let fx = (lon + 180.0) / 360.0;
    let s = lat.to_radians().sin().clamp(-0.9999, 0.9999);
    let fy = 0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * std::f64::consts::PI);
    (fx, fy)
}

/// 瓦片四角经纬度（west, north, east, south）。
fn tile_bounds(key: Key, n: f64) -> Option<(f64, f64, f64, f64)> {
    let lon0 = key.x as f64 / n * 360.0 - 180.0;
    let lon1 = (key.x + 1) as f64 / n * 360.0 - 180.0;
    let lat0 = tile_lat(key.y as f64 / n);
    let lat1 = tile_lat((key.y + 1) as f64 / n);
    if !lat0.is_finite() || !lat1.is_finite() {
        return None;
    }
    Some((lon0, lat0, lon1, lat1))
}

fn tile_lat(fy: f64) -> f64 {
    let n = std::f64::consts::PI - 2.0 * std::f64::consts::PI * fy;
    n.sinh().atan().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_roundtrip() {
        let (fx, fy) = lonlat_to_tile(121.540241, 38.901678, 16, 65536.0);
        // 反算经纬度
        let lon = fx * 360.0 - 180.0;
        let lat = tile_lat(fy);
        assert!((lon - 121.540241).abs() < 1e-9, "lon={lon}");
        assert!((lat - 38.901678).abs() < 1e-9, "lat={lat}");
    }

    fn layer() -> TileLayer {
        TileLayer::new(
            true,
            "https://basemaps.cartocdn.com/rastertiles/voyager/{z}/{x}/{y}.png",
            Datum::Wgs84,
        )
    }

    /// 视野内的瓦片必须真的落在画布上——这是"底图一片空白"的回归测试。
    #[test]
    fn visible_tiles_cover_the_canvas() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1290.0, 420.0));
        // 手工核对一张瓦片：几何换算是否正确
        let n = 2f64.powi(18);
        let key = Key {
            z: 18,
            x: 213_338,
            y: 109_664,
        };
        let (lon0, lat0, lon1, lat1) = tile_bounds(key, n).unwrap();
        let (c_lat, c_lon) = location::to_bd09(28.188_424, 112.975_708, Datum::Gcj02);
        let map = MapState {
            center: (c_lat, c_lon),
            scale: 1.88,
            ..Default::default()
        };
        let (bl, bo) = location::to_bd09(lat0, lon0, Datum::Wgs84);
        let p0 = map.to_screen(rect, bl, bo);
        let (bl2, bo2) = location::to_bd09(lat1, lon1, Datum::Wgs84);
        let p1 = map.to_screen(rect, bl2, bo2);
        println!(
            "tile bounds lon {lon0:.5}..{lon1:.5} lat {lat1:.5}..{lat0:.5}; screen {p0:?} -> {p1:?}"
        );
        for scale in [0.05_f32, 0.3, 1.88, 8.0] {
            let map = MapState {
                center: (c_lat, c_lon),
                scale,
                ..Default::default()
            };
            let tiles = layer().visible(&map, rect);
            assert!(!tiles.is_empty(), "scale={scale} 没算出任何瓦片");
            // 视野中心必须被某张瓦片覆盖
            let c = rect.center();
            assert!(
                tiles.iter().any(|(_, r)| r.contains(c)),
                "scale={scale} 中心没被瓦片覆盖: {:?}",
                tiles.iter().take(3).collect::<Vec<_>>()
            );
            // 瓦片在屏幕上不至于小到看不见
            for (_, r) in tiles.iter().take(4) {
                assert!(r.width() > 20.0 && r.height() > 20.0, "scale={scale} 瓦片过小 {r:?}");
            }
        }
    }

    /// 放大后缩放级别要变大（更细）。
    #[test]
    fn zoom_tracks_scale() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, 400.0));
        let small = MapState {
            center: (38.9, 121.54),
            scale: 0.3,
            ..Default::default()
        };
        let big = MapState {
            center: (38.9, 121.54),
            scale: 1.2,
            ..Default::default()
        };
        let z_small = layer().visible(&small, rect)[0].0.z;
        let z_big = layer().visible(&big, rect)[0].0.z;
        assert!((14..=16).contains(&z_small), "z={z_small}");
        assert!(z_big > z_small, "z={z_small} z_big={z_big}");
    }

    /// 子域占位符要被替换（高德那种 wprd0{1-4} 写法）。
    #[test]
    fn subdomain_placeholder_is_expanded() {
        let layer = TileLayer::new(
            true,
            "https://wprd0{1-4}.is.autonavi.com/appmaptile?x={x}&y={y}&z={z}",
            Datum::Gcj02,
        );
        let key = Key { z: 17, x: 26, y: 41 };
        let sub = (key.x + key.y) % 4 + 1;
        let url = layer
            .url_template
            .replace("{1-4}", &sub.to_string())
            .replace("{z}", &key.z.to_string())
            .replace("{x}", &key.x.to_string())
            .replace("{y}", &key.y.to_string());
        assert_eq!(
            url,
            "https://wprd04.is.autonavi.com/appmaptile?x=26&y=41&z=17"
        );
        assert!(!url.contains('{'), "URL 还有没替换的占位符: {url}");
    }

    #[test]
    fn tile_bounds_are_ordered() {
        let n = 2f64.powi(15);
        let (lon0, lat0, lon1, lat1) = tile_bounds(
            Key {
                z: 15,
                x: 26_900,
                y: 12_100,
            },
            n,
        )
        .unwrap();
        assert!(lon0 < lon1);
        assert!(lat0 > lat1, "纬度应自上而下递减");
    }

    /// 真实网络检查：底图 URL/UA/PNG 解码整条链路。
    ///
    /// `cargo test live_tile_fetch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_tile_fetch() {
        let url = "https://basemaps.cartocdn.com/rastertiles/voyager/15/26900/12100.png";
        let agent = crate::api::client::make_agent();
        let mut body = Vec::new();
        agent
            .get(url)
            .set("User-Agent", TILE_UA)
            .call()
            .expect("瓦片请求失败")
            .into_reader()
            .read_to_end(&mut body)
            .expect("读取瓦片失败");
        assert!(
            body.starts_with(&[0x89, b'P', b'N', b'G']),
            "不是 PNG，实际前 8 字节: {:?}",
            &body[..body.len().min(8)]
        );
        let img = image::load_from_memory(&body).expect("PNG 解码失败");
        println!("瓦片 OK {}x{} {} bytes", img.width(), img.height(), body.len());
    }
}
