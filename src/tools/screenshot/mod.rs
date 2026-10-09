//! 全屏截图：捕获主显示器 → 预览 → 复制到剪贴板 / 存 PNG。
//!
//! 只有纯逻辑，不碰 GPUI，方便单元测试。Windows 下 xcap 走 GDI 全屏捕获；
//! 多显示器只截主显示器。区域框选见同目录的 `overlay`，矩形数学见 `select`。

pub mod overlay;
pub mod select;
pub mod view;

// 外壳持有截图状态，走工具根路径拿，不直接钻 view。
pub(crate) use view::ShotState;

use std::borrow::Cow;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// 预览图最大边（逻辑像素思路：够看清、不占内存）。
pub const PREVIEW_MAX_W: u32 = 640;
pub const PREVIEW_MAX_H: u32 = 400;

/// 截主显示器全屏，返回 RGBA 像素。
pub fn capture_primary() -> Result<image::RgbaImage, String> {
    primary_monitor()?
        .capture_image()
        .map_err(|e| format!("截图失败: {e}"))
}

fn primary_monitor() -> Result<xcap::Monitor, String> {
    xcap::Monitor::all()
        .map_err(|e| format!("枚举显示器失败: {e}"))?
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .ok_or_else(|| "没找到主显示器".to_string())
}

/// 主显示器几何（物理像素 + 缩放），覆盖层定位用。
#[derive(Debug, Clone, Copy)]
pub struct MonitorGeometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub scale: f32,
}

/// 主显示器几何。缩放读不到时按 1.0 算（选择框和裁剪按同比例换算，
/// 差的只是 HiDPI 下的清晰度，不会错位）。
pub fn primary_geometry() -> Result<MonitorGeometry, String> {
    let m = primary_monitor()?;
    Ok(MonitorGeometry {
        x: m.x().map_err(|e| format!("读显示器位置失败: {e}"))?,
        y: m.y().map_err(|e| format!("读显示器位置失败: {e}"))?,
        w: m.width().map_err(|e| format!("读显示器尺寸失败: {e}"))?,
        h: m.height().map_err(|e| format!("读显示器尺寸失败: {e}"))?,
        scale: m.scale_factor().unwrap_or(1.0),
    })
}

impl MonitorGeometry {
    /// 逻辑像素界（物理 / 缩放）：GPUI 窗口坐标和鼠标事件都用这套。
    pub fn logical(&self) -> (f32, f32, f32, f32) {
        (
            self.x as f32 / self.scale,
            self.y as f32 / self.scale,
            self.w as f32 / self.scale,
            self.h as f32 / self.scale,
        )
    }
}

/// RgbaImage 编成 PNG 字节（预览和落盘都用它）。
pub fn encode_png(img: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| format!("编码 PNG 失败: {e}"))?;
    Ok(buf)
}

/// 等比缩略到界限内再编 PNG，给预览用（原图太大不直接进界面）。
pub fn thumbnail_png(img: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let thumb = image::imageops::thumbnail(img, PREVIEW_MAX_W, PREVIEW_MAX_H);
    encode_png(&thumb)
}

/// 原图存 PNG（扩展名决定格式，调用方保证是 `.png`）。
pub fn save_png(img: &image::RgbaImage, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败: {e}"))?;
    }
    img.save(path).map_err(|e| format!("存文件失败: {e}"))?;
    Ok(())
}

/// 原图写入系统剪贴板。arboard 要的 RGBA 字节这里现拷一份
/// （1080p 约 8MB，一次性的复制操作可以接受）。
pub fn copy_image(img: &image::RgbaImage) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("打开剪贴板失败: {e}"))?;
    let data = arboard::ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: Cow::Owned(img.clone().into_raw()),
    };
    clipboard
        .set_image(data)
        .map_err(|e| format!("写入剪贴板失败: {e}"))?;
    Ok(())
}

/// 图片库下的 WongTools 目录（`%USERPROFILE%\Pictures\WongTools`）。
pub fn pictures_dir() -> PathBuf {
    let pictures = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Pictures");
    pictures.join("WongTools")
}

/// 按 Unix 秒拼文件名（ASCII 安全，不用处理中文路径的幺蛾子）。
pub fn timestamped_name() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("screenshot-{secs}.png")
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::MonitorGeometry;
    use super::copy_image;
    use super::encode_png;
    use super::save_png;
    use super::thumbnail_png;
    use super::timestamped_name;

    /// 2x2 纯色测试图，不依赖显示器。
    fn test_image() -> image::RgbaImage {
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
    }

    #[test]
    fn encode_png_writes_png_magic() {
        let bytes = encode_png(&test_image()).expect("能编码");
        assert!(
            bytes.starts_with(&[137, 80, 78, 71]),
            "PNG 魔数不对，编出来的不是 PNG"
        );
    }

    #[test]
    fn thumbnail_stays_within_bounds() {
        let big = image::RgbaImage::from_pixel(100, 50, image::Rgba([0, 255, 0, 255]));
        let bytes = thumbnail_png(&big).expect("能缩略");
        let thumb = image::load_from_memory(&bytes).expect("缩略图能解码");
        assert!(
            thumb.width() <= 640 && thumb.height() <= 400,
            "缩略图超界"
        );
        assert!(thumb.width() > thumb.height(), "等比关系坏了");
    }

    #[test]
    fn save_png_writes_readable_file() {
        let path =
            std::env::temp_dir().join(format!("wong-tools-shot-{}.png", std::process::id()));
        save_png(&test_image(), &path).expect("能存");
        let bytes = std::fs::read(&path).expect("能读回");
        assert!(!bytes.is_empty(), "存了个空文件");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn timestamped_name_is_ascii_png() {
        let name = timestamped_name();
        assert!(name.is_ascii(), "文件名必须 ASCII 安全");
        assert!(name.ends_with(".png"), "扩展名必须是 .png");
    }

    #[test]
    fn logical_divides_physical_by_scale() {
        // 纯数学，不碰显示器：4K 物理 @2x 缩放 = 1920x1080 逻辑。
        let geo = MonitorGeometry {
            x: 0,
            y: 0,
            w: 3840,
            h: 2160,
            scale: 2.0,
        };
        assert_eq!(geo.logical(), (0., 0., 1920., 1080.));
    }

    #[test]
    fn copy_image_roundtrips_through_clipboard() {
        // 要桌面会话；CI 无头环境这里会挂，见 AGENTS（截图测试要桌面）。
        copy_image(&test_image()).expect("能写入剪贴板");
        let mut clipboard = arboard::Clipboard::new().expect("能打开剪贴板");
        let back = clipboard.get_image().expect("能读回");
        assert_eq!((back.width, back.height), (2, 2), "剪贴板图片尺寸不对");
    }

    #[test]
    fn capture_primary_returns_real_pixels() {
        // 要桌面会话；无头环境这里会挂，见 AGENTS。
        let img = super::capture_primary().expect("能截到主显示器");
        assert!(img.width() > 0 && img.height() > 0, "截了个空图");
        assert_eq!(
            img.len(),
            (img.width() * img.height() * 4) as usize,
            "RGBA 字节数不对"
        );
    }

    #[test]
    fn capture_is_not_a_black_frame() {
        // 真桌面不可能是纯黑：平均亮度几乎为 0 说明 GDI 截回来的是空帧，
        // 后面的显示、裁剪再对都没用。采样 4096 个点，快。
        let img = super::capture_primary().expect("能截到主显示器");
        let pixels = img.as_raw();
        let step = (pixels.len() / 4 / 4096).max(1) * 4;
        let (mut sum, mut n) = (0u64, 0u64);
        let mut i = 0;
        while i + 2 < pixels.len() {
            sum += pixels[i] as u64 + pixels[i + 1] as u64 + pixels[i + 2] as u64;
            n += 1;
            i += step;
        }
        let mean = sum as f64 / (n * 3) as f64;
        assert!(
            mean > 4.0,
            "截回来是纯黑帧（平均亮度 {mean:.2}），GDI 在这台机器上没拿到画面"
        );
    }
}
