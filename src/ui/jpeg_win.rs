//! 用 Windows GDI+ 解码 JPEG。
//!
//! 背景：本项目的 `image` 依赖只开了 `png` 特性（离线 vendor 里没有 zune-jpeg），
//! 而卫星影像瓦片（Esri World Imagery、高德 webst 等）一律是 JPEG。
//! GDI+ 是系统自带组件（gdiplus.dll），用它把 JPEG 转成 RGBA 位图，
//! 不需要引入任何新依赖。
//!
//! 只做「文件 → 像素」这一件事：把字节写到临时文件，`GdipCreateBitmapFromFile`
//! 读入，`GdipBitmapLockBits` 取出 32bpp ARGB 像素，转成 egui 的 `ColorImage`。

#![cfg(windows)]

use eframe::egui::ColorImage;
use std::os::windows::ffi::OsStrExt;

#[repr(C)]
struct GdiplusStartupInput {
    gdiplus_version: u32,
    debug_event_callback: *mut std::ffi::c_void,
    suppress_background_thread: i32,
    suppress_external_codecs: i32,
}

#[repr(C)]
struct GdiplusRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[repr(C)]
struct BitmapData {
    width: u32,
    height: u32,
    stride: i32,
    pixel_format: i32,
    scan0: *mut u8,
    reserved: usize,
}

#[link(name = "gdiplus")]
extern "system" {
    fn GdiplusStartup(
        token: *mut usize,
        input: *const GdiplusStartupInput,
        output: *mut std::ffi::c_void,
    ) -> i32;
    fn GdipCreateBitmapFromFile(
        filename: *const u16,
        bitmap: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn GdipGetImageWidth(image: *mut std::ffi::c_void, width: *mut u32) -> i32;
    fn GdipGetImageHeight(image: *mut std::ffi::c_void, height: *mut u32) -> i32;
    fn GdipBitmapLockBits(
        bitmap: *mut std::ffi::c_void,
        rect: *const GdiplusRect,
        flags: u32,
        format: i32,
        data: *mut BitmapData,
    ) -> i32;
    fn GdipBitmapUnlockBits(bitmap: *mut std::ffi::c_void, data: *mut BitmapData) -> i32;
    fn GdipDisposeImage(image: *mut std::ffi::c_void) -> i32;
}

/// `PixelFormat32bppARGB`（GDI+ 里非预乘的 32 位 ARGB）。
const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026_200A;
const IMAGE_LOCK_MODE_READ: u32 = 1;

fn ensure_startup() -> Result<(), String> {
    static STATUS: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    let status = *STATUS.get_or_init(|| {
        let input = GdiplusStartupInput {
            gdiplus_version: 1,
            debug_event_callback: std::ptr::null_mut(),
            suppress_background_thread: 0,
            suppress_external_codecs: 0,
        };
        let mut token: usize = 0;
        unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) }
    });
    if status == 0 {
        Ok(())
    } else {
        Err(format!("GDI+ 初始化失败（status={status}）"))
    }
}

/// 解码 JPEG 字节为 RGBA 图像。
pub fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
    ensure_startup()?;
    // GDI+ 的 Flat API 没有「从内存流解码」的简单入口（要 COM IStream），
    // 用临时文件最省事；解完立刻删除。系统临时目录不可写时退到数据目录。
    let name = format!("neko-tile-{}.jpg", uuid::Uuid::new_v4());
    let candidates = [
        std::env::temp_dir().join(&name),
        crate::platform::data_dir().join(&name),
    ];
    let mut last_err = String::from("没有可写目录");
    for path in candidates {
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                let result = decode_file(&path);
                let _ = std::fs::remove_file(&path);
                return result;
            }
            Err(e) => last_err = format!("写临时文件失败({}): {e}", path.display()),
        }
    }
    Err(last_err)
}

fn decode_file(path: &std::path::Path) -> Result<ColorImage, String> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut bitmap: *mut std::ffi::c_void = std::ptr::null_mut();
    let status = unsafe { GdipCreateBitmapFromFile(wide.as_ptr(), &mut bitmap) };
    if status != 0 || bitmap.is_null() {
        return Err(format!("GdipCreateBitmapFromFile 失败（status={status}）"));
    }

    let mut width = 0u32;
    let mut height = 0u32;
    unsafe {
        GdipGetImageWidth(bitmap, &mut width);
        GdipGetImageHeight(bitmap, &mut height);
    }
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        unsafe { GdipDisposeImage(bitmap) };
        return Err(format!("图像尺寸异常：{width}×{height}"));
    }

    let rect = GdiplusRect {
        x: 0,
        y: 0,
        width: width as i32,
        height: height as i32,
    };
    let mut data = BitmapData {
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
        scan0: std::ptr::null_mut(),
        reserved: 0,
    };
    let status = unsafe {
        GdipBitmapLockBits(
            bitmap,
            &rect,
            IMAGE_LOCK_MODE_READ,
            PIXEL_FORMAT_32BPP_ARGB,
            &mut data,
        )
    };
    if status != 0 || data.scan0.is_null() {
        unsafe { GdipDisposeImage(bitmap) };
        return Err(format!("GdipBitmapLockBits 失败（status={status}）"));
    }

    let (w, h, stride) = (width as usize, height as usize, data.stride as usize);
    let mut pixels = Vec::with_capacity(w * h * 4);
    unsafe {
        for row in 0..h {
            let row_ptr = data.scan0.add(row * stride);
            for col in 0..w {
                let p = row_ptr.add(col * 4);
                // GDI+ 32bppARGB 是小端 BGRA
                let (b, g, r, a) = (*p, *p.add(1), *p.add(2), *p.add(3));
                pixels.extend_from_slice(&[r, g, b, a]);
            }
        }
        GdipBitmapUnlockBits(bitmap, &mut data);
        GdipDisposeImage(bitmap);
    }
    Ok(ColorImage::from_rgba_unmultiplied([w, h], &pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_jpeg_without_crashing() {
        // 不是 JPEG：GDI+ 会返回失败状态，我们不能 panic
        assert!(decode(b"not an image").is_err());
    }

    /// 真实网络检查：抓一张 Esri 卫星瓦片并解码。
    ///
    /// `cargo test live_jpeg_decode -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_jpeg_decode() {
        let url = "https://server.arcgisonline.com/ArcGIS/rest/services/\
                   World_Imagery/MapServer/tile/18/109598/213403";
        let agent = crate::api::client::make_agent();
        let mut body = Vec::new();
        use std::io::Read;
        agent
            .get(url)
            .call()
            .expect("请求瓦片失败")
            .into_reader()
            .read_to_end(&mut body)
            .expect("读取失败");
        assert_eq!(body[..2], [0xFF, 0xD8], "不是 JPEG");
        let img = decode(&body).expect("GDI+ 解码失败");
        assert_eq!(img.size, [256, 256]);
        // 卫星图应该有比较丰富的颜色（不是纯色）
        let distinct = img
            .pixels
            .iter()
            .step_by(97)
            .map(|p| (p.r(), p.g(), p.b()))
            .collect::<std::collections::HashSet<_>>()
            .len();
        println!("解码成功 {}x{}，采样到 {} 种颜色", img.size[0], img.size[1], distinct);
        assert!(distinct > 20, "图像像是纯色，可能解错了");
    }
}
