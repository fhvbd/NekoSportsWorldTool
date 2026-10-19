//! 运行位置的统一数据结构、坐标基准换算与校验。
//!
//! 设备锚点、点位请求和跑步记录使用同一套纬度/经度语义；服务端点位（打卡点）
//! 一律是 BD-09，而用户从手机地图抄来的坐标可能是 WGS84（GPS 原始）或 GCJ-02
//! （高德/腾讯）。`SimLocation` 记录「用户填的基准 + 数值」，使用时统一换算成
//! BD-09，避免手工换算方向搞反。

use serde::{Deserialize, Serialize};

const DEFAULT_ANCHOR_LAT: f64 = 38.901678;
const DEFAULT_ANCHOR_LON: f64 = 121.540241;
const DEFAULT_CITY: &str = "大连市";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Coordinate {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy: f64,
}

impl Coordinate {
    pub fn new(latitude: f64, longitude: f64, accuracy: f64) -> Result<Self, String> {
        let coordinate = Self {
            latitude,
            longitude,
            accuracy,
        };
        coordinate.validate()?;
        Ok(coordinate)
    }

    pub fn validate(self) -> Result<(), String> {
        if !self.latitude.is_finite() || !(-90.0..=90.0).contains(&self.latitude) {
            return Err(format!("纬度超出范围: {}", self.latitude));
        }
        if !self.longitude.is_finite() || !(-180.0..=180.0).contains(&self.longitude) {
            return Err(format!("经度超出范围: {}", self.longitude));
        }
        if !self.accuracy.is_finite() || self.accuracy < 0.0 {
            return Err(format!("定位精度无效: {}", self.accuracy));
        }
        Ok(())
    }

    pub fn is_near(self, other: Self, tolerance: f64) -> bool {
        (self.latitude - other.latitude).abs() <= tolerance
            && (self.longitude - other.longitude).abs() <= tolerance
    }

    pub fn is_default_dalian(self) -> bool {
        self.is_near(
            Self {
                latitude: DEFAULT_ANCHOR_LAT,
                longitude: DEFAULT_ANCHOR_LON,
                accuracy: 0.0,
            },
            1e-6,
        )
    }
}

pub fn default_anchor() -> Coordinate {
    Coordinate {
        latitude: DEFAULT_ANCHOR_LAT,
        longitude: DEFAULT_ANCHOR_LON,
        accuracy: 0.0,
    }
}

pub fn default_city() -> &'static str {
    DEFAULT_CITY
}

// ── 坐标基准 ────────────────────────────────────────────────────────

/// 用户填写坐标时使用的基准。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Datum {
    /// GPS 原始坐标（手机自带的「经纬度」、OSM、Google 海外）。
    Wgs84,
    /// 火星坐标（高德、腾讯、Apple 国内地图）。
    Gcj02,
    /// 百度坐标（百度地图；服务端点位与提交轨迹使用该基准）。
    #[default]
    Bd09,
}

impl Datum {
    pub const ALL: [Datum; 3] = [Datum::Wgs84, Datum::Gcj02, Datum::Bd09];

    pub fn code(self) -> &'static str {
        match self {
            Datum::Wgs84 => "wgs84",
            Datum::Gcj02 => "gcj02",
            Datum::Bd09 => "bd09",
        }
    }

    /// 界面与文档里的短名。
    pub fn label(self) -> &'static str {
        match self {
            Datum::Wgs84 => "WGS84",
            Datum::Gcj02 => "GCJ-02",
            Datum::Bd09 => "BD-09",
        }
    }

    /// 界面上的一句话说明（避免用户搞不清该选哪个）。
    pub fn hint(self) -> &'static str {
        match self {
            Datum::Wgs84 => "GPS 原始坐标",
            Datum::Gcj02 => "高德 / 腾讯地图",
            Datum::Bd09 => "百度地图（服务端基准）",
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let key = text.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|d| d.code() == key || d.label().eq_ignore_ascii_case(&key))
            .ok_or_else(|| format!("未知坐标基准: {text}（可选 wgs84 / gcj02 / bd09）"))
    }
}

/// 任意基准 → BD-09（服务端点位请求与轨迹使用的基准）。
pub fn to_bd09(lat: f64, lon: f64, datum: Datum) -> (f64, f64) {
    match datum {
        Datum::Bd09 => (lat, lon),
        Datum::Gcj02 => crate::track::geom::gcj02_to_bd09(lat, lon),
        Datum::Wgs84 => crate::track::geom::wgs84_to_bd09(lat, lon),
    }
}

/// BD-09 → 任意基准（界面展示 / 用户对照地图）。
pub fn from_bd09(lat: f64, lon: f64, datum: Datum) -> (f64, f64) {
    match datum {
        Datum::Bd09 => (lat, lon),
        Datum::Gcj02 => crate::track::wire::bd09_to_gcj02(lat, lon),
        Datum::Wgs84 => crate::track::geom::bd09_to_wgs84(lat, lon),
    }
}

/// 解析用户粘贴的坐标对，接受 `38.9,121.5` / `38.9 121.5` / `38.9;121.5`。
///
/// 手机地图常把经度放前面：|第一项| > 90 时自动认定为「经度,纬度」并交换；
/// 两项都 ≤ 90 时无法判定，按「纬度,经度」处理（界面会给出提示）。
pub fn parse_pair(text: &str) -> Result<(f64, f64), String> {
    let cleaned = text.replace(['，', '、', '；'], ",");
    let parts: Vec<&str> = cleaned
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() != 2 {
        return Err("请提供两个数字：纬度、经度（例如 38.901678,121.540241）".into());
    }
    let first = parts[0]
        .parse::<f64>()
        .map_err(|_| format!("无法解析为数字: {}", parts[0]))?;
    let second = parts[1]
        .parse::<f64>()
        .map_err(|_| format!("无法解析为数字: {}", parts[1]))?;
    if !first.is_finite() || !second.is_finite() {
        return Err("坐标必须是有限数字".into());
    }
    let (lat, lon) = if first.abs() > 90.0 && second.abs() <= 90.0 {
        (second, first)
    } else {
        (first, second)
    };
    let coordinate = Coordinate::new(lat, lon, 0.0)?;
    Ok((coordinate.latitude, coordinate.longitude))
}

// ── 用户自定义的模拟位置 ─────────────────────────────────────────────

/// 用户自行设定的模拟定位锚点（config.json 持久化）。
///
/// 与设备信息页的 `anchor_lat/anchor_lon` 的区别：这里是「本次跑步想落在哪」，
/// 带坐标基准与城市/备注；启用后覆盖设备锚点，且跑步流程不再自动漂移锚点。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SimLocation {
    #[serde(default)]
    pub enabled: bool,
    /// 数值按 `datum` 记录的纬度。
    pub lat: f64,
    /// 数值按 `datum` 记录的经度。
    pub lon: f64,
    /// `lat/lon` 的坐标基准。
    #[serde(default)]
    pub datum: Datum,
    /// 提交记录 `address` 字段使用的城市名。
    #[serde(default)]
    pub city: String,
    /// 收藏夹里的显示名（可为空）。
    #[serde(default)]
    pub label: String,
}

impl SimLocation {
    /// 由用户输入构造（会校验范围并裁剪城市/备注首尾空白）。
    pub fn new(lat: f64, lon: f64, datum: Datum) -> Result<Self, String> {
        let location = Self {
            enabled: true,
            lat,
            lon,
            datum,
            city: String::new(),
            label: String::new(),
        };
        location.validate()?;
        Ok(location)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.lat.is_finite() || !(-90.0..=90.0).contains(&self.lat) {
            return Err(format!("纬度超出范围: {}", self.lat));
        }
        if !self.lon.is_finite() || !(-180.0..=180.0).contains(&self.lon) {
            return Err(format!("经度超出范围: {}", self.lon));
        }
        Ok(())
    }

    /// 换算到 BD-09（协议基准）。
    pub fn bd09(&self) -> Result<(f64, f64), String> {
        self.validate()?;
        Ok(to_bd09(self.lat, self.lon, self.datum))
    }

    /// 本次跑步使用的锚点（`Coordinate` 是 BD-09 语义）。
    pub fn anchor(&self) -> Result<Coordinate, String> {
        let (lat, lon) = self.bd09()?;
        Coordinate::new(lat, lon, 0.0)
    }

    /// 按指定基准显示坐标。
    pub fn point_in(&self, datum: Datum) -> Result<(f64, f64), String> {
        let (lat, lon) = self.bd09()?;
        Ok(from_bd09(lat, lon, datum))
    }

    /// 换基准并保留物理位置（界面切换单选项时用）。
    pub fn reexpressed(&self, datum: Datum) -> Result<Self, String> {
        let (lat, lon) = self.point_in(datum)?;
        Ok(Self {
            datum,
            lat,
            lon,
            ..self.clone()
        })
    }

    /// 「备注 / 城市 / 坐标」的一行摘要。
    pub fn summary(&self) -> String {
        format!(
            "{}（{} {:.6},{:.6}）",
            self.short_name(),
            self.datum.label(),
            self.lat,
            self.lon
        )
    }

    /// 界面用的短名：备注 > 城市 > 默认。
    pub fn short_name(&self) -> &str {
        if !self.label.trim().is_empty() {
            self.label.trim()
        } else if !self.city.trim().is_empty() {
            self.city.trim()
        } else {
            "模拟位置"
        }
    }

    /// 城市名（空则回退设备城市）。
    pub fn city_or<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.city.trim().is_empty() {
            fallback
        } else {
            self.city.trim()
        }
    }
}

/// 由一组 BD-09 点算外接矩形，向外扩 `margin_m` 米，返回 `[min_lat, min_lon, max_lat, max_lon]`。
///
/// 用来把地图视野锁定在学校范围（打卡点云 + 余量）内。
pub fn bbox_from_points(points: &[(f64, f64)], margin_m: f64) -> Option<[f64; 4]> {
    if points.is_empty() {
        return None;
    }
    let mut min_lat = f64::INFINITY;
    let mut max_lat = f64::NEG_INFINITY;
    let mut min_lon = f64::INFINITY;
    let mut max_lon = f64::NEG_INFINITY;
    for &(lat, lon) in points {
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        min_lon = min_lon.min(lon);
        max_lon = max_lon.max(lon);
    }
    if !min_lat.is_finite() || !min_lon.is_finite() {
        return None;
    }
    let d_lat = margin_m / crate::track::geom::MET_PER_DEG_LAT;
    let d_lon = margin_m / crate::track::geom::MET_PER_DEG_LNG;
    Some([
        min_lat - d_lat,
        min_lon - d_lon,
        max_lat + d_lat,
        max_lon + d_lon,
    ])
}

/// 矩形的中心点（BD-09）。
pub fn bbox_center(bbox: [f64; 4]) -> (f64, f64) {
    (
        (bbox[0] + bbox[2]) / 2.0,
        (bbox[1] + bbox[3]) / 2.0,
    )
}

/// 矩形的边长（米，东西 × 南北）。
pub fn bbox_size_m(bbox: [f64; 4]) -> (f64, f64) {
    (
        (bbox[3] - bbox[1]) * crate::track::geom::MET_PER_DEG_LNG,
        (bbox[2] - bbox[0]) * crate::track::geom::MET_PER_DEG_LAT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounds_and_accuracy() {
        assert!(Coordinate::new(39.9, 116.4, 5.0).is_ok());
        assert!(Coordinate::new(91.0, 116.4, 5.0).is_err());
        assert!(Coordinate::new(39.9, 181.0, 5.0).is_err());
        assert!(Coordinate::new(39.9, 116.4, -1.0).is_err());
    }

    #[test]
    fn detects_legacy_default_location() {
        assert!(default_anchor().is_default_dalian());
        assert!(!Coordinate::new(39.9, 116.4, 0.0)
            .unwrap()
            .is_default_dalian());
    }

    #[test]
    fn parses_pasted_pairs_in_both_orders() {
        // 纬度,经度
        assert_eq!(
            parse_pair("38.901678,121.540241").unwrap(),
            (38.901678, 121.540241)
        );
        // 空格 / 中文逗号 / 分号
        assert_eq!(
            parse_pair("38.901678 121.540241").unwrap(),
            (38.901678, 121.540241)
        );
        assert_eq!(
            parse_pair("38.901678，121.540241").unwrap(),
            (38.901678, 121.540241)
        );
        assert_eq!(
            parse_pair(" 38.901678 ; 121.540241 ").unwrap(),
            (38.901678, 121.540241)
        );
        // 经度在前（|第一项| > 90）自动交换
        assert_eq!(
            parse_pair("121.540241,38.901678").unwrap(),
            (38.901678, 121.540241)
        );
        // 负数经度同样按幅值判定
        assert_eq!(parse_pair("-122.4,37.7").unwrap(), (37.7, -122.4));
    }

    #[test]
    fn rejects_malformed_pairs() {
        assert!(parse_pair("").is_err());
        assert!(parse_pair("38.9").is_err());
        assert!(parse_pair("38.9,121.5,1.0").is_err());
        assert!(parse_pair("abc,121.5").is_err());
        assert!(parse_pair("91,121.5").is_err());
        assert!(parse_pair("38.9,181").is_err());
    }

    #[test]
    fn datum_parsing_is_forgiving() {
        assert_eq!(Datum::parse("wgs84").unwrap(), Datum::Wgs84);
        assert_eq!(Datum::parse("GCJ-02").unwrap(), Datum::Gcj02);
        assert_eq!(Datum::parse("bd09").unwrap(), Datum::Bd09);
        assert_eq!(Datum::parse(" BD-09 ").unwrap(), Datum::Bd09);
        assert!(Datum::parse("mars").is_err());
    }

    /// 三套基准互转：同一物理点的 BD-09 锚点一致（误差 < 0.2 m）。
    #[test]
    fn datum_roundtrip_keeps_the_same_physical_point() {
        let bd = (38.901678, 121.540241);
        for datum in Datum::ALL {
            let (lat, lon) = from_bd09(bd.0, bd.1, datum);
            let sim = SimLocation::new(lat, lon, datum).unwrap();
            let back = sim.bd09().unwrap();
            let d_lat = (back.0 - bd.0) * crate::track::geom::MET_PER_DEG_LAT;
            let d_lon = (back.1 - bd.1) * crate::track::geom::MET_PER_DEG_LNG;
            assert!(
                (d_lat * d_lat + d_lon * d_lon).sqrt() < 0.2,
                "{datum:?} 往返偏移过大: {d_lat:.3}/{d_lon:.3} m"
            );
        }
    }

    /// 换基准保留物理位置（界面切换单选项不该让点跳走）。
    #[test]
    fn reexpressing_keeps_the_point() {
        let sim = SimLocation::new(38.901678, 121.540241, Datum::Bd09).unwrap();
        let wgs = sim.reexpressed(Datum::Wgs84).unwrap();
        assert_eq!(wgs.datum, Datum::Wgs84);
        assert!((wgs.lat - 38.901678).abs() > 1e-4, "基准换算应改变数值");
        let back = wgs.reexpressed(Datum::Bd09).unwrap();
        assert!((back.lat - sim.lat).abs() < 1e-6, "lat={}", back.lat);
        assert!((back.lon - sim.lon).abs() < 1e-6, "lon={}", back.lon);
    }

    #[test]
    fn anchor_rejects_out_of_range_values() {
        let mut sim = SimLocation::new(38.9, 121.5, Datum::Bd09).unwrap();
        assert!(sim.anchor().is_ok());
        sim.lat = 95.0;
        assert!(sim.anchor().is_err());
        sim.lat = 38.9;
        sim.lon = f64::NAN;
        assert!(sim.anchor().is_err());
    }

    /// config.json 兼容：旧文件没有 sim_location 字段时按 None 读取。
    #[test]
    fn sim_location_survives_serde_roundtrip() {
        let sim = SimLocation {
            enabled: true,
            lat: 39.9,
            lon: 116.4,
            datum: Datum::Gcj02,
            city: "北京市".into(),
            label: "操场".into(),
        };
        let text = serde_json::to_string(&sim).unwrap();
        let back: SimLocation = serde_json::from_str(&text).unwrap();
        assert_eq!(sim, back);
        // 缺省字段走默认值
        let sparse: SimLocation = serde_json::from_str(r#"{"lat":1.0,"lon":2.0}"#).unwrap();
        assert!(!sparse.enabled);
        assert_eq!(sparse.datum, Datum::Bd09);
    }

    #[test]
    fn summary_prefers_label_then_city() {        let mut sim = SimLocation::new(38.9, 121.5, Datum::Bd09).unwrap();
        assert!(sim.summary().starts_with("模拟位置"));
        sim.city = "大连市".into();
        assert!(sim.summary().starts_with("大连市"));
        sim.label = "主操场".into();
        assert!(sim.summary().starts_with("主操场"));
        assert_eq!(sim.city_or("某某市"), "大连市");
        sim.city = "   ".into();
        assert_eq!(sim.city_or("某某市"), "某某市");
    }

    /// 校园外接框：覆盖所有点，并按米数外扩。
    #[test]
    fn bbox_covers_points_with_margin() {
        let pts = vec![(28.267307, 113.063832), (28.267967, 113.065512)];
        let bbox = bbox_from_points(&pts, 150.0).expect("bbox");
        assert!(bbox[0] < 28.267307);
        assert!(bbox[2] > 28.267967);
        assert!(bbox[1] < 113.063832);
        assert!(bbox[3] > 113.065512);
        // 外扩 150 m 后高度约 73 m + 300 m，宽度约 145 m + 300 m
        let (w, h) = bbox_size_m(bbox);
        assert!((h - 373.0).abs() < 5.0, "h={h}");
        assert!((w - 445.0).abs() < 5.0, "w={w}");
        // 中心在两点之间
        let (lat, lon) = bbox_center(bbox);
        assert!(lat > 28.2673 && lat < 28.2680, "lat={lat}");
        assert!(lon > 113.0638 && lon < 113.0656, "lon={lon}");
        assert!(bbox_from_points(&[], 10.0).is_none());
    }
}
