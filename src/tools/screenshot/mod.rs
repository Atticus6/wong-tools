//! 全屏截图：捕获主显示器 → 预览 → 复制到剪贴板 / 存 PNG。
//!
//! 只有纯逻辑，不碰 GPUI，方便单元测试。Windows 下 xcap 走 GDI 全屏捕获；
//! 多显示器只截主显示器。区域框选见同目录的 `overlay`，矩形数学见 `select`。

pub mod annotate;
pub mod history;
pub mod ocr;
pub mod overlay;
pub mod pin;
pub mod select;
pub mod view;

// 外壳持有截图状态，走工具根路径拿，不直接钻 view。
pub(crate) use view::ShotState;

use std::borrow::Cow;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// 截主显示器全屏，返回 RGBA 像素。
pub fn capture_primary() -> Result<image::RgbaImage, String> {
    // macOS 先验屏幕录制权限：没授权时底下拿到的只是纯壁纸（其他窗口全缺），
    // 而 xcap 自己从不弹授权框。`request` 在未决定时弹系统框，已拒绝时直接
    // 返回 false，随后给去系统设置的指引（未签名包从 /Applications 启动，
    // 别在 DMG 里直接跑，否则授权落不下）。
    #[cfg(target_os = "macos")]
    {
        let access = core_graphics::access::ScreenCaptureAccess::default();
        if !access.preflight() {
            access.request();
            if !access.preflight() {
                return Err("没有屏幕录制权限，截到的只是壁纸：系统设置 → 隐私与安全性 → 屏幕录制 → 勾选 WongTools（从 /Applications 里启动），允许后重试"
                    .into());
            }
        }
    }
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

/// 主显示器几何，覆盖层定位用。
///
/// `w`/`h` 是**逻辑像素**（点），`scale` 是物理/逻辑比（HiDPI 为 2.0）。
/// 注意 xcap 各平台语义不同：macOS 的 `Monitor::width/height` 走
/// `CGDisplayBounds`，本来就是逻辑点；Windows 走 `dmPelsWidth`，是物理像素。
/// 这里统一在读入时归一到逻辑点，下游（含 `logical()`）就只当逻辑点用，
/// 免得再按平台各扣一次缩放——扣错就是覆盖层缩一半。
#[derive(Debug, Clone, Copy)]
pub struct MonitorGeometry {
    /// 全局坐标原点（逻辑点，macOS 全局系以主屏左上为原点）。
    pub x: i32,
    pub y: i32,
    /// 逻辑宽高（点）。
    pub w: u32,
    pub h: u32,
    /// 物理/逻辑比，捕获图转物理像素时用。
    pub scale: f32,
}

/// 主显示器几何。缩放读不到时按 1.0 算（选择框和裁剪按同比例换算，
/// 差的只是 HiDPI 下的清晰度，不会错位）。
pub fn primary_geometry() -> Result<MonitorGeometry, String> {
    let m = primary_monitor()?;
    let scale = m.scale_factor().unwrap_or(1.0).max(0.01);
    // macOS 直接就是逻辑点；Windows 是物理像素，换算成逻辑点。
    #[cfg(target_os = "macos")]
    let (w, h) = (
        m.width().map_err(|e| format!("读显示器尺寸失败: {e}"))?,
        m.height().map_err(|e| format!("读显示器尺寸失败: {e}"))?,
    );
    #[cfg(not(target_os = "macos"))]
    let (w, h) = (
        (m.width().map_err(|e| format!("读显示器尺寸失败: {e}"))? as f32 / scale).round() as u32,
        (m.height().map_err(|e| format!("读显示器尺寸失败: {e}"))? as f32 / scale).round() as u32,
    );
    Ok(MonitorGeometry {
        x: m.x().map_err(|e| format!("读显示器位置失败: {e}"))?,
        y: m.y().map_err(|e| format!("读显示器位置失败: {e}"))?,
        w,
        h,
        scale,
    })
}

impl MonitorGeometry {
    /// 逻辑像素界 `(x, y, w, h)`：GPUI 窗口坐标和鼠标事件都用这套。
    /// `w`/`h` 入库时已归一为逻辑点，这里原样返回，不再除缩放。
    pub fn logical(&self) -> (f32, f32, f32, f32) {
        (
            self.x as f32,
            self.y as f32,
            self.w as f32,
            self.h as f32,
        )
    }
}

/// 覆盖层在主屏上的落位：窗口矩形 + 顶部被系统占掉的逻辑像素。
///
/// macOS 不允许普通窗口盖住菜单栏，`open_window` 要的全屏矩形会被系统悄悄
/// 挪到菜单栏下沿（实测 1x 屏 pos=(0,25)、Retina 屏 pos=(0,38)，而高度仍
/// 按原样给足）。冻结帧本身含菜单栏那几行，整幅跟着下移后屏幕上就出现两
/// 个菜单栏，底部还少一截看不见。Windows 任务栏在下沿，顶边不内缩，行为
/// 与修复前完全一致。
///
/// 差量只认顶边：Dock 在下沿，用整块 `visible_bounds` 反而会把 Dock 排除
/// 在选区外，截不到它。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayPlacement {
    /// 覆盖层窗口矩形（逻辑像素，全局坐标）。
    pub origin: (f32, f32),
    /// 覆盖层窗口尺寸（逻辑像素）。
    pub size: (f32, f32),
    /// 顶边被系统保留区（macOS 菜单栏）吃掉的高度，逻辑像素。
    pub top_inset: f32,
}

/// 按显示器的全屏界和可见界算覆盖层落位。
///
/// `full` / `visible` 都是 `(x, y, w, h)` 逻辑像素，取自 GPUI 的
/// `Display::bounds()` / `visible_bounds()`（同一坐标系）。
///
/// **只认 visible 的顶边**：macOS 的 `visible_bounds` 顶边被菜单栏压下去，
/// 底边又被 Dock 抬上来，两边一减就成了「屏幕减去上下装饰」那块，套上去
/// 会把 Dock 排除在选区外截不到。顶边差值单独取才是菜单栏的真实高度。
/// 负数（理论上不会出现）一律收成 0。
pub fn overlay_placement(
    full: (f32, f32, f32, f32),
    visible: (f32, f32, f32, f32),
) -> OverlayPlacement {
    let (fx, fy, fw, fh) = full;
    let top_inset = (visible.1 - fy).clamp(0., fh);
    OverlayPlacement {
        origin: (fx, fy + top_inset),
        size: (fw, (fh - top_inset).max(0.)),
        top_inset,
    }
}

/// RgbaImage 编成 PNG 字节（缩略图落盘前走内存，单测也靠它验编码）。
pub fn encode_png(img: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| format!("编码 PNG 失败: {e}"))?;
    Ok(buf)
}

/// 等比适配：把 `w`×`h` 按原比例收进 `bound_w`×`bound_h` 框，
/// 返回目标尺寸（宽高都 ≥1）。注意 `image::imageops::thumbnail`
/// **不保比例**（实测 800×400 进 480×480 框出来还是 480×480，
/// 直接拉伸变形），所以调用方先用它算出等比尺寸再缩。
/// 小图也放大到贴框（和原来行为一致，列里不留空洞）。
pub fn fit_within(w: u32, h: u32, bound_w: u32, bound_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 || bound_w == 0 || bound_h == 0 {
        return (1, 1);
    }
    let scale = (bound_w as f64 / w as f64).min(bound_h as f64 / h as f64);
    let tw = ((w as f64 * scale).round() as u32).max(1);
    let th = ((h as f64 * scale).round() as u32).max(1);
    (tw, th)
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

/// 图片库下的 WongTools 目录：用户可见、可直接管理截图。
/// Windows 走 `%USERPROFILE%\Pictures`，macOS 走 `~/Pictures`，
/// Linux 走 `$XDG_PICTURES_DIR`（没有则 `~/Pictures`）。历史截图和手动
/// 保存都落在这里（历史另起子目录，见 `history::history_dir`）。
pub fn pictures_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Pictures");
    #[cfg(target_os = "macos")]
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Pictures");
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("Pictures")
        });
    base.join("WongTools")
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
    use super::fit_within;
    use super::overlay_placement;
    use super::pictures_dir;
    use super::save_png;
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
    fn fit_within_keeps_ratio_and_stays_in_box() {
        // 宽图贴宽边、高图贴高边，另一边按比例收，绝不超框。
        assert_eq!(fit_within(800, 400, 640, 400), (640, 320));
        assert_eq!(fit_within(400, 800, 640, 400), (200, 400));
        assert_eq!(fit_within(100, 100, 480, 480), (480, 480));
        // 零尺寸不除零，给 1×1 占位（调用方不会拿零尺寸进来）。
        assert_eq!(fit_within(0, 100, 480, 480), (1, 1));
    }

    #[test]
    fn pictures_dir_is_visible_library_not_appdata() {
        // 手动保存和历史都进用户图片库，文件管理器直接看得到。
        assert!(
            pictures_dir().ends_with("WongTools"),
            "应用图片目录名别乱改"
        );
        assert!(
            !pictures_dir().to_string_lossy().contains("shot-history"),
            "pictures_dir 本体别带历史子目录，历史另挂一层"
        );
    }

    #[test]
    fn logical_passes_through_already_normalized_points() {
        // 入库已归一成逻辑点，logical() 不该再除缩放。
        // 2x 屏：逻辑 1470x956 就是 1470x956（早期版本误除 2，overlay 缩一半）。
        let geo = MonitorGeometry {
            x: 0,
            y: 0,
            w: 1470,
            h: 956,
            scale: 2.0,
        };
        assert_eq!(geo.logical(), (0., 0., 1470., 956.));
    }

    #[test]
    fn logical_keeps_scale_for_capture_conversion() {
        // 缩放仍要留着给 `to_pixels`：逻辑选区 × scale 才是捕获图的物理像素。
        let geo = MonitorGeometry {
            x: 0,
            y: 0,
            w: 1470,
            h: 956,
            scale: 2.0,
        };
        assert_eq!(geo.scale, 2.0);
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
    fn placement_shifts_window_below_macos_menu_bar() {
        // macOS 实测（1x 屏）：全屏 (0,0,1920,1080)，可见区顶边被菜单栏压到 25。
        // 覆盖层必须自己让出这 25，否则系统再推一次就出两个菜单栏。
        let p = overlay_placement((0., 0., 1920., 1080.), (0., 25., 1920., 1023.));
        assert_eq!(p.top_inset, 25.);
        assert_eq!(p.origin, (0., 25.));
        // 高度是「全屏减内缩」，不是直接用 visible 的高：Dock 在下沿，
        // 用 visible 会把 Dock 排除在选区外。
        assert_eq!(p.size, (1920., 1055.));
    }

    #[test]
    fn placement_handles_retina_menu_bar_height() {
        // Retina 实测：逻辑 1470x956，菜单栏 38pt（捕获图是 2940x1912 物理）。
        // 内缩跟缩放无关，只认逻辑点的顶边差。
        let p = overlay_placement((0., 0., 1470., 956.), (0., 38., 1470., 886.));
        assert_eq!(p.top_inset, 38.);
        assert_eq!(p.origin, (0., 38.));
        assert_eq!(p.size, (1470., 918.));
    }

    #[test]
    fn placement_is_identity_when_menu_bar_absent() {
        // Windows 任务栏在下沿：可见区顶边 == 全屏顶边，内缩 0，行为不变。
        let p = overlay_placement((0., 0., 1920., 1080.), (0., 0., 1920., 1040.));
        assert_eq!(p.top_inset, 0.);
        assert_eq!(p.origin, (0., 0.));
        assert_eq!(p.size, (1920., 1080.));
    }

    #[test]
    fn placement_clamps_inset_to_screen_height() {
        // 读不到显示器 / 多屏坐标系错乱时不该算出负高度或负内缩。
        let p = overlay_placement((0., 0., 1920., 1080.), (0., -50., 1920., 1080.));
        assert_eq!(p.top_inset, 0.);
        assert_eq!(p.size.1, 1080.);
        // 内缩大于屏高（脏数据）也不能出负高度。
        let p = overlay_placement((0., 0., 1920., 1080.), (0., 5000., 1920., 10.));
        assert_eq!(p.top_inset, 1080.);
        assert_eq!(p.size.1, 0.);
    }

    #[test]
    fn placement_offsets_for_non_zero_display_origin() {
        // 非主屏 / 副屏在主屏右边时，横坐标要跟着显示器走，不能写死 0。
        let p = overlay_placement((1920., 0., 2560., 1440.), (1920., 25., 2560., 1383.));
        assert_eq!(p.top_inset, 25.);
        assert_eq!(p.origin, (1920., 25.));
        assert_eq!(p.size, (2560., 1415.));
    }

    #[test]
    fn placement_keeps_window_bottom_at_screen_bottom() {
        // 渲染的对齐前提：冻结帧按 -top_inset 上移后第 0 行落在窗口顶，
        // 而窗口底必须正好贴屏幕底，否则底部那截（Dock）露白或被切掉。
        let p = overlay_placement((0., 0., 1920., 1080.), (0., 25., 1920., 1023.));
        assert_eq!(
            p.origin.1 + p.size.1,
            1080.,
            "窗口底没贴屏幕底，底部会露白"
        );
        // 冻结帧整幅高 = 窗口高 + 内缩：盖住窗口还得多出菜单栏那么高。
        assert_eq!(p.size.1 + p.top_inset, 1080.);
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
