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
        "Carto Voyager（默认，PNG）",
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
    (
        "高德（中文，GCJ-02，注意对方条款）",
        "https://wprd0{1-4}.is.autonavi.com/appmaptile?lang=zh_cn&size=1&scale=1&style=8&x={x}&y={y}&z={z}",
        Datum::Gcj02,
    ),
];

/// 当前模板对应哪个预设（用于下拉框回显）。
pub fn preset_index(url: &str) -> Option<usize> {
    PRESETS.iter().position(|(_, u, _)| *u == url)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key {
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
    pub fn draw(&mut self, ui: &mut egui::Ui, map: &MapState, rect: Rect) -> bool {
        if !self.enabled {
            return false;
        }
        self.drain(ui.ctx());
        let Some((z, range)) = self.view(map, rect) else {
            return false;
        };
        self.zoom = z;
        let painter = ui.painter_at(rect);
        let n = 2f64.powi(z as i32);
        let mut painted = false;
        for (x, y) in range {
            let key = Key { z, x, y };
            let Some((lon0, lat0, lon1, lat1)) = tile_bounds(key, n) else {
                continue;
            };
            // 瓦片坐标系 → BD-09 → 屏幕
            let (bd_lat0, bd_lon0) = location::to_bd09(lat0, lon0, self.datum);
            let (bd_lat1, bd_lon1) = location::to_bd09(lat1, lon1, self.datum);
            let p0 = map.to_screen(rect, bd_lat0, bd_lon0);
            let p1 = map.to_screen(rect, bd_lat1, bd_lon1);
            let tile_rect = Rect::from_min_max(
                egui::pos2(p0.x.min(p1.x), p0.y.min(p1.y)),
                egui::pos2(p0.x.max(p1.x), p0.y.max(p1.y)),
            );
            if !tile_rect.intersects(rect) {
                continue;
            }
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

    /// 计算当前视图需要的缩放级别与瓦片范围。
    fn view(&self, map: &MapState, rect: Rect) -> Option<(u8, Vec<(u32, u32)>)> {
        let lat = map.center.0;
        if !lat.is_finite() {
            return None;
        }
        // 目标分辨率 √(m/px) = 1/scale，用墨卡托公式反推 z
        let scale = map.scale.max(0.02) as f64;
        let z = (156_543.033_92 * lat.to_radians().cos() * scale).log2().round();
        let z = z.clamp(2.0, 19.0) as u8;
        let n = 2f64.powi(z as i32);
        let (fx0, fy0) = lonlat_to_tile(map.center.1, lat, z, n);
        let half_w = rect.width() as f64 / 2.0 / TILE_PX;
        let half_h = rect.height() as f64 / 2.0 / TILE_PX;
        // 反向缩放（地图 scale 与瓦片原生分辨率不完全相等时按比例换算）
        let px_per_tile = TILE_PX * (1.0 / scale)
            * (156_543.033_92 * lat.to_radians().cos() / n);
        let _ = px_per_tile;
        let x0 = (fx0 - half_w - map.offset.x as f64 / TILE_PX).floor() as i64;
        let x1 = (fx0 + half_w - map.offset.x as f64 / TILE_PX).ceil() as i64;
        let y0 = (fy0 - half_h - map.offset.y as f64 / TILE_PX).floor() as i64;
        let y1 = (fy0 + half_h - map.offset.y as f64 / TILE_PX).ceil() as i64;
        let mut keys = Vec::new();
        for y in y0..=y1 {
            for x in x0..=x1 {
                if y < 0 || y as f64 >= n {
                    continue;
                }
                let xw = x.rem_euclid(n as i64) as u32;
                keys.push((xw, y as u32));
                if keys.len() >= MAX_TILES_PER_VIEW {
                    return Some((z, keys));
                }
            }
        }
        Some((z, keys))
    }

    fn request(&mut self, key: Key) {
        if self.inflight.contains(&key) || self.failed.contains(&key) {
            return;
        }
        if self.inflight.len() >= MAX_INFLIGHT || self.requested >= MAX_TILES_TOTAL {
            return;
        }
        let url = self
            .url_template
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

    #[test]
    fn zoom_tracks_scale() {
        // 3.3 m/px（scale≈0.3）在纬度 39 附近应落在 z15 左右
        let map = MapState {
            center: (38.9, 121.54),
            scale: 0.3,
            ..Default::default()
        };
        let layer = TileLayer::new(true, "https://tile.openstreetmap.org/{z}/{x}/{y}.png", Datum::Wgs84);
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, 400.0));
        let (z, keys) = layer.view(&map, rect).expect("view");
        assert!((14..=16).contains(&z), "z={z}");
        assert!(!keys.is_empty());
        // 放大 4 倍应多两级左右
        let map2 = MapState {
            center: (38.9, 121.54),
            scale: 1.2,
            ..Default::default()
        };
        let (z2, _) = layer.view(&map2, rect).expect("view");
        assert!(z2 > z, "z={z} z2={z2}");
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
