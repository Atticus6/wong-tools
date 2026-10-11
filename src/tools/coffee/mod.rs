//! 咖啡工具：端起咖啡防休眠，盖上盖子放行。
//!
//! 只有纯逻辑（时长预设、文案格式、防休眠守卫），不碰 GPUI，好单测。
//! 视图和状态在 `view`；跨模块拿状态走工具根路径（`CoffeeView` 重导出）。
//! 防休眠按平台三套做法（原因见 `KeepAwake`）：Windows 调
//! `SetThreadExecutionState`（kernel32 直连，不引入新依赖）；macOS 挂
//! `caffeinate` 子进程（IOKit 断言要手写 CF 类型，直调不如子进程稳）；
//! Linux 挂 `systemd-inhibit`（非 systemd 环境直接报错，不静默假装）。

pub mod view;

// 外壳持有咖啡状态，走工具根路径拿，不直接钻 view。
pub(crate) use view::CoffeeView;

use crate::i18n::t;

/// 时长预设（分钟，`None` = 常开）：界面按表渲染，加档只改这里。
pub const PRESETS: &[Option<u64>] = &[Some(15), Some(30), Some(60), Some(120), Some(240), None];
/// 默认时长 1 小时：没选过、存档坏了都回落到它。
pub const DEFAULT_MINUTES: Option<u64> = Some(60);
/// 自定义时长上限 24 小时：再长就是常开的事，别手填了。
pub const MAX_CUSTOM_MINUTES: u64 = 1440;

/// 剩余秒 → `1:02:03`（满一小时）或 `12:34`（不足一小时不带小时段）。
/// 纯函数，倒计时展示用，单测覆盖。
pub fn format_remaining(total_secs: u64) -> String {
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

/// 自定义分钟解析：只要 1..=1440 的整数，其他一律拒绝。
/// 纯函数，输入框入库前验，单测覆盖。
pub fn parse_custom_minutes(raw: &str) -> Result<u64, ()> {
    raw.trim().parse::<u64>().ok().filter(|m| *m >= 1 && *m <= MAX_CUSTOM_MINUTES).ok_or(())
}

/// 预设标签（走当前语言）：`15 分钟` / `15 min`，`1 小时` / `1 hr`，
/// 常开 `None` 走 `coffee_no_limit`。英文小时统一 `hr`，没有单复数问题。
pub fn preset_label(minutes: Option<u64>) -> String {
    let Some(m) = minutes else {
        // “常开”
        return t("app.coffee_no_limit");
    };
    if m % 60 == 0 {
        // “{} 小时” / “{} hr”
        t("app.coffee_hour_fmt").replace("{}", &(m / 60).to_string())
    } else {
        // “{} 分钟” / “{} min”
        t("app.coffee_min_fmt").replace("{}", &m.to_string())
    }
}

/// 剩余行文案：文案里的 `{}` 换成时间（中文前置 `剩余 {}`，英文后置 `{} left`）。
pub fn remaining_text(total_secs: u64) -> String {
    // “剩余 {}” / “{} left”
    t("app.coffee_remaining").replace("{}", &format_remaining(total_secs))
}

// ── resvg 杯子 ──────────────────────────────────────────────
// 杯子是手写的 SVG（`cup_svg`），经 resvg 烤成 RGBA 内存图（`render_cup_rgba`），
// 再经截图覆盖层同款 `rgba_to_render_image` 换成 BGRA 直显：整条路零 PNG 编解码，
// 新帧上传是同步的，不闪（之前每帧换新 PNG 走异步解码，杯子一直在闪刷）。
// resvg 只会渲染静态图，动画靠逐帧重烤：蒸汽是三缕相位错开的雾气，
// 每帧按相位算位移 + 透明度，合盖帧与帧号无关（静态，烤一次复用）。
// 配色跟主题走（`dark` 来自 `cx.theme().mode`）：深色主题用哑光深咖杯，
// 浅色用米白杯，糊在背景里不跳。

/// 杯子视图逻辑尺寸：SVG 按它画，烤 2 倍给 HiDPI，界面按它摆。
pub const CUP_VIEW_W: f32 = 220.0;
/// 杯子视图逻辑尺寸（蒸汽区 + 杯身 + 杯托全在里面）。
pub const CUP_VIEW_H: f32 = 244.0;
/// 烤图倍率：2 倍，位图里 440x488。
pub const CUP_SCALE: u32 = 2;
/// 蒸汽帧步进：120ms 一帧，20 帧一轮（约 2.4 秒飘完一缕）。
const STEAM_STEP: f64 = 0.05;
/// 蒸汽缕数：三缕，相位三等分错开。
const STEAM_WISPS: usize = 3;

/// 一套杯子配色：线条扁平只有三样东西——描边色、咖啡液、蒸汽，
/// 杯身/杯托/盖子全镂空（`fill="none"`，只留描边），造型靠线条。
/// 深浅两套造型完全一样，只换颜色，换主题不用改画法。
struct CupPalette {
    /// 描边（杯身/杯耳/杯口/杯托/盖子/豆标共用）。
    line: &'static str,
    /// 咖啡液（唯一的实心大色块，压住画面）。
    coffee: &'static str,
    /// 蒸汽。
    steam: &'static str,
}

/// 浅色主题：棕色线条，浅底上轮廓分明。
const LIGHT_PALETTE: CupPalette = CupPalette {
    line: "#7a6547",
    coffee: "#4a2c17",
    steam: "#9aa3ad",
};

/// 深色主题：浅驼色线条。深底上线条必须是亮的（深色线直接 invisible），
/// 咖啡液用最深的棕，靠杯口那圈亮线收边。
const DARK_PALETTE: CupPalette = CupPalette {
    line: "#d8ab72",
    coffee: "#1f1206",
    steam: "#c3c9d1",
};

/// 按主题拿调色板：纯函数，单测两套都烤。
fn palette(dark: bool) -> &'static CupPalette {
    if dark { &DARK_PALETTE } else { &LIGHT_PALETTE }
}

/// 烤一帧杯子位图（sRGB 非预乘 RGBA，调用方转 BGRA 直显）。
/// `running` 开盖冒烟 / 合盖，`frame` 只对开盖有效（蒸汽相位），
/// 合盖传什么都一样；`dark` 跟当前主题。
/// 失败返回展示给用户的中文原因（SVG 是程序拼的，正常走不到失败分支）。
pub fn render_cup_rgba(running: bool, frame: u64, dark: bool) -> Result<image::RgbaImage, String> {
    let svg = cup_svg(running, frame, dark);
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())
        // “杯子画失败了”
        .map_err(|e| format!("杯子画失败了：{e}"))?;
    let (w, h) = (
        (CUP_VIEW_W as u32) * CUP_SCALE,
        (CUP_VIEW_H as u32) * CUP_SCALE,
    );
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(w, h).ok_or_else(|| "杯子画失败了：开不出画布".to_string())?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
    // tiny-skia 存的是预乘 alpha，`RgbaImage` 要的是非预乘：这里拆一次，
    // 不拆半透明蒸汽会发灰（BGRA 转换在覆盖层那边，见 `rgba_to_render_image`）。
    Ok(unpremultiply(&pixmap))
}

/// 预乘转非预乘：alpha 为 0 的像素直接置透明，其他按 `c*255/a` 还原。
fn unpremultiply(pixmap: &resvg::tiny_skia::Pixmap) -> image::RgbaImage {
    let mut buf = Vec::with_capacity((pixmap.width() * pixmap.height() * 4) as usize);
    for px in pixmap.pixels() {
        let a = px.alpha();
        if a == 0 {
            buf.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            // 四舍五入（+a/2），255 封顶，数学上不会溢出，纯粹防手滑。
            let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
            buf.extend_from_slice(&[un(px.red()), un(px.green()), un(px.blue()), a]);
        }
    }
    // 长度是自己按 w*h*4 推出来的，不可能对不上；对不上说明上面写错了，
    // 直接 panic 比吞掉强（测试会先抓到）。
    image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), buf)
        .expect("unpremultiply 的字节数必须对上画布")
}

/// 杯子 SVG（简约线条）：描边画形，杯身全镂空，唯一的实心是咖啡液。
/// 简约 = 元素最少：豆标、杯托内圈、盖箍、饮口全不要，盖子只剩盖板 + 盖钮；
/// 杯身收窄拉高，线宽统一 3（杯耳 8、蒸汽 3.5）。开盖是液面 + 三缕蒸汽，
/// 合盖是盖子 + 盖钮。`frame` 只驱动蒸汽相位（合盖忽略），
/// `dark` 换整套配色（造型不动）。图层都带 id（`open-top` / `lid` /
/// `steam`），单测按它断开合状态。
pub fn cup_svg(running: bool, frame: u64, dark: bool) -> String {
    let p = palette(dark);
    let top = if running {
        format!(
            "<g id=\"open-top\">\
            <ellipse cx=\"120\" cy=\"112\" rx=\"54\" ry=\"12\" fill=\"none\" stroke=\"{line}\" stroke-width=\"3\"/>\
            <ellipse cx=\"120\" cy=\"112\" rx=\"42\" ry=\"8.5\" fill=\"{coffee}\"/>\
            <g id=\"steam\">{}</g></g>",
            steam_wisps(frame, p),
            line = p.line,
            coffee = p.coffee,
        )
    } else {
        format!(
            "<g id=\"lid\">\
            <rect x=\"107\" y=\"72\" width=\"26\" height=\"14\" rx=\"6\" fill=\"none\" stroke=\"{line}\" stroke-width=\"3\"/>\
            <rect x=\"58\" y=\"85\" width=\"124\" height=\"28\" rx=\"10\" fill=\"none\" stroke=\"{line}\" stroke-width=\"3\"/></g>",
            line = p.line,
        )
    };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"440\" height=\"488\" viewBox=\"0 0 220 244\">\
        <ellipse cx=\"120\" cy=\"224\" rx=\"72\" ry=\"11\" fill=\"none\" stroke=\"{line}\" stroke-width=\"3\"/>\
        <path d=\"M169,130 C197,130 197,180 157,184\" fill=\"none\" stroke=\"{line}\" stroke-width=\"8\" stroke-linecap=\"round\"/>\
        <path d=\"M66,112 L174,112 L161,204 Q160,216 148,216 L92,216 Q80,216 79,204 Z\" fill=\"none\" stroke=\"{line}\" stroke-width=\"3\" stroke-linejoin=\"round\"/>\
        {top}</svg>",
        line = p.line,
    )
}

/// 三缕蒸汽：细波浪线，按相位 `p`（0 新生 → 1 消散）算上浮、
/// 1 消散）算上浮、摇摆和透明度。新生淡入、顶部消散，差一帧也连续。
/// 活动范围验算：底座 y=98，上浮 28，缕高 68，p=1 时顶端 y=2，
/// 永不顶出视图上沿（顶出去就是一刀切的断头，看着像 bug）。
/// 蛇形摆幅 ±18：描边细、摆幅大，缩略看也是弯的（摆幅小了糊成直柱）。
fn steam_wisps(frame: u64, p: &CupPalette) -> String {
    let mut out = String::new();
    for i in 0..STEAM_WISPS {
        // 三缕相位三等分：第 0 缕跟帧走，另外两缕错开 1/3、2/3 轮。
        let p_phase = (frame as f64 * STEAM_STEP + i as f64 / STEAM_WISPS as f64) % 1.0;
        // 上浮 28：从液面（y≈98）飘到 y≈70，底端叠进杯口也无妨
        // （透明度到顶已归零，看不见穿帮）。
        let ty = 98.0 - p_phase * 28.0;
        // 左右摇摆 ±7，缕与缕之间再错半个周期，不 parallel。
        let tx = [98.0, 120.0, 142.0][i]
            + 7.0 * (std::f64::consts::TAU * (p_phase + i as f64 * 0.5)).sin();
        // 新生 15% 内淡入，之后随上升线性消散。
        let opacity = 0.55 * (p_phase / 0.15).min(1.0) * (1.0 - p_phase);
        out.push_str(&format!(
            "<path d=\"M0,0 C-18,-11 18,-23 0,-34 C-18,-45 18,-57 0,-68\" \
            transform=\"translate({tx:.1},{ty:.1})\" \
            fill=\"none\" stroke=\"{steam}\" stroke-width=\"4\" stroke-linecap=\"round\" \
            opacity=\"{opacity:.3}\"/>",
            steam = p.steam,
        ));
    }
    out
}

/// 防休眠守卫：活着就不休眠，`drop` 即放行（Windows 复位执行状态，
/// 其他平台杀掉子进程）。`WsShell::drop` 里顺手清掉，退出不留断言。
pub struct KeepAwake {
    /// 非 Windows：子进程守着（macOS 是 `caffeinate`，Linux 是
    /// `systemd-inhibit`），drop 时杀掉；Windows 无字段（状态在系统侧）。
    #[cfg(not(windows))]
    child: std::process::Child,
}

impl KeepAwake {
    /// 子进程 PID（启动清理认领孤儿用）：macOS 是 `caffeinate`，
    /// Linux 是 `systemd-inhibit`。Windows 没子进程，不提供这个方法
    /// （断言跟进程走，不可能残留，记 PID 纯属多余）。
    #[cfg(not(windows))]
    pub(crate) fn child_pid(&self) -> u32 {
        self.child.id()
    }

    /// 拿断言。`display` 为 true 时屏幕也不熄（macOS 多挂 `-d`，
    /// Windows 多带 `ES_DISPLAY_REQUIRED`）；默认只保主机不睡。
    /// 失败返回展示给用户的中文原因（带系统原话，方便定位）。
    pub fn acquire(display: bool) -> Result<Self, String> {
        #[cfg(windows)]
        {
            win::acquire(display).map(|()| Self {})
        }
        #[cfg(target_os = "macos")]
        {
            // `-i` 防闲置休眠（主机不睡）；`-d` 另防显示器熄。
            // 不用 `-u`：它会把已熄的屏点亮，太打扰。
            // 输出全扔黑洞：子进程活多久都别往控制台吐字。
            let mut args = vec!["-i"];
            if display {
                args.push("-d");
            }
            std::process::Command::new("caffeinate")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                // “防休眠启动失败”
                .map(|child| Self { child })
                .map_err(|e| format!("{}：{e}", t("app.coffee_failed")))
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            // `sleep infinity` 占着 inhibit 锁，杀掉即放行。
            // 非 systemd 环境这里直接报错，不静默假装防住了。
            std::process::Command::new("systemd-inhibit")
                .args([
                    "--what=sleep:idle",
                    "--who=WongTools",
                    "--why=coffee",
                    "sleep",
                    "infinity",
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                // “防休眠启动失败”
                .map(|child| Self { child })
                .map_err(|e| format!("{}：{e}", t("app.coffee_failed")))
        }
    }
}

/// Windows：`SetThreadExecutionState` 语义是进程级的（`ES_CONTINUOUS`
/// 让状态一直有效到下一次调用），调一次就行，不用后台线程续命。
#[cfg(windows)]
mod win {
    /// 一直有效 + 主机不睡。
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    /// 主机不睡。
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    /// 显示器不熄。
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    // kernel32 直连：就一个函数，不值得为它引入 windows crate。
    #[link(name = "kernel32")]
    extern "system" {
        fn SetThreadExecutionState(es_flags: u32) -> u32;
    }

    /// 举断言：返回 0 说明系统拒绝（极少见，直接报给用户）。
    pub(super) fn acquire(display: bool) -> Result<(), String> {
        let mut flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
        if display {
            flags |= ES_DISPLAY_REQUIRED;
        }
        // SAFETY：纯整数参数，无指针、无回调，失败返回 0 不碰内存。
        let prev = unsafe { SetThreadExecutionState(flags) };
        if prev == 0 {
            // “防休眠启动失败”
            Err(format!("{}：SetThreadExecutionState", crate::i18n::t("app.coffee_failed")))
        } else {
            Ok(())
        }
    }

    /// 放行：只带 `ES_CONTINUOUS` 即清掉之前的要求。
    pub(super) fn release() {
        // SAFETY：同上，复位调用不可能失败出内存问题。
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

#[cfg(windows)]
impl Drop for KeepAwake {
    /// 退出兜底：`WsShell::drop` 正常走这里，被杀才留给系统回收
    /// （执行状态跟进程走，进程没了断言自动失效）。
    fn drop(&mut self) {
        win::release();
    }
}

#[cfg(not(windows))]
impl Drop for KeepAwake {
    /// 退出兜底：杀掉子进程（`caffeinate` / `systemd-inhibit` 退出即放行），
    /// 再 `wait` 收尸，免得留僵尸进程。
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::MAX_CUSTOM_MINUTES;
    use super::PRESETS;
    use super::cup_svg;
    use super::format_remaining;
    use super::parse_custom_minutes;
    use super::preset_label;
    use super::remaining_text;
    use super::render_cup_rgba;
    use crate::i18n::{LANGUAGES, Language, lock_locale, set_language};
    use crate::settings::TempSettings;

    #[test]
    fn remaining_formats_hours_only_when_needed() {
        // 不足一小时 `MM:SS`，满一小时 `H:MM:SS`，0 秒不崩。
        assert_eq!(format_remaining(0), "00:00");
        assert_eq!(format_remaining(59), "00:59");
        assert_eq!(format_remaining(60), "01:00");
        assert_eq!(format_remaining(15 * 60), "15:00");
        assert_eq!(format_remaining(3600), "1:00:00");
        assert_eq!(format_remaining(2 * 3600 + 3 * 60 + 7), "2:03:07");
    }

    #[test]
    fn custom_minutes_accepts_only_sane_range() {
        // 正常值、前后空格都认；0、超限、瞎写的一律拒绝。
        assert_eq!(parse_custom_minutes("90"), Ok(90));
        assert_eq!(parse_custom_minutes("  30 "), Ok(30));
        assert_eq!(parse_custom_minutes("1"), Ok(1));
        assert_eq!(parse_custom_minutes(&MAX_CUSTOM_MINUTES.to_string()), Ok(MAX_CUSTOM_MINUTES));
        assert!(parse_custom_minutes("0").is_err(), "0 分钟没有意义");
        assert!(parse_custom_minutes("1441").is_err(), "超 24 小时请用常开");
        assert!(parse_custom_minutes("").is_err());
        assert!(parse_custom_minutes("abc").is_err());
        assert!(parse_custom_minutes("-5").is_err());
        assert!(parse_custom_minutes("1.5").is_err());
    }

    #[test]
    fn presets_cover_common_lengths_and_never_expire() {
        // 预设表从刻钟到常开，加档只改 `PRESETS`，这里锁死防手滑删档。
        assert_eq!(PRESETS, &[Some(15), Some(30), Some(60), Some(120), Some(240), None]);
    }

    #[test]
    fn cup_svg_holds_exactly_one_top_layer() {
        // 开盖：有蒸汽层和液面，没有盖子；合盖反之（图层 id 是契约）。
        // 深浅两套都验：造型一样，只换颜色。
        for dark in [false, true] {
            let open = cup_svg(true, 0, dark);
            assert!(open.contains("id=\"steam\""), "开盖必须有蒸汽层");
            assert!(open.contains("id=\"open-top\""), "开盖必须有液面层");
            assert!(!open.contains("id=\"lid\""), "开盖不能有盖子");
            let closed = cup_svg(false, 0, dark);
            assert!(closed.contains("id=\"lid\""), "合盖必须有盖子");
            assert!(!closed.contains("id=\"steam\""), "合盖不能有蒸汽");
        }
        // 深浅配色不一样（不然跟主题走就是空话）。
        assert_ne!(cup_svg(true, 0, false), cup_svg(true, 0, true));
        // 合盖与帧号无关：静态帧，烤一次复用就行。
        assert_eq!(cup_svg(false, 0, false), cup_svg(false, 99, false));
        // 开盖随帧变：蒸汽相位在走。
        assert_ne!(cup_svg(true, 0, false), cup_svg(true, 5, false));
    }

    #[test]
    fn cup_frame_renders_expected_bitmap() {
        // 烤出来是 2 倍视图位图、不是纯色（杯和透明底肯定不一样）。
        // resvg 纯 CPU，单测直接烤；深浅两套都验。
        for dark in [false, true] {
            for running in [false, true] {
                let img = render_cup_rgba(running, 0, dark).expect("杯子必须烤出来");
                assert_eq!((img.width(), img.height()), (440, 488), "2 倍视图尺寸");
                let first = img.pixels().next().expect("有像素").0;
                assert!(
                    img.pixels().any(|px| px.0 != first),
                    "纯色说明 SVG 没画上东西"
                );
            }
        }
    }

    #[test]
    fn steam_animates_but_lid_stays_put() {
        // 开盖连烤两帧不一样（蒸汽在飘）；合盖连烤两帧像素级一致
        // （静态，界面复用缓存不重烤）。
        let a = render_cup_rgba(true, 0, false).expect("能烤");
        let b = render_cup_rgba(true, 7, false).expect("能烤");
        assert_ne!(a.as_raw(), b.as_raw(), "蒸汽不动就是动画坏了");
        let c = render_cup_rgba(false, 0, false).expect("能烤");
        let d = render_cup_rgba(false, 42, false).expect("能烤");
        assert_eq!(c.as_raw(), d.as_raw(), "合盖还每帧重烤就是浪费");
    }

    #[test]
    fn preset_and_remaining_labels_follow_locale() {
        // 标签走当前语言：改全局 locale 的测试要串行，切语言会写库，
        // 指向临时文件。
        let _guard = lock_locale();
        let _settings = TempSettings::install("coffee-labels");
        let prev = crate::i18n::language();
        for lang in LANGUAGES {
            set_language(*lang);
            let unlimited = preset_label(None);
            let quarter = preset_label(Some(15));
            let hour = preset_label(Some(60));
            let remaining = remaining_text(61);
            assert!(!unlimited.is_empty() && unlimited != "app.coffee_no_limit");
            assert!(!quarter.is_empty() && quarter != "app.coffee_min_fmt");
            assert!(!hour.is_empty() && hour != "app.coffee_hour_fmt");
            assert!(remaining.contains("01:01"), "时间必须拼进文案：{remaining}");
            if *lang == Language::Chinese {
                // “常开” / “15 分钟” / “1 小时” / “剩余 01:01”
                assert_eq!(unlimited, "常开");
                assert_eq!(quarter, "15 分钟");
                assert_eq!(hour, "1 小时");
                assert_eq!(remaining, "剩余 01:01");
            } else {
                assert_eq!(unlimited, "No limit");
                assert_eq!(quarter, "15 min");
                assert_eq!(hour, "1 hr");
                assert_eq!(remaining, "01:01 left");
            }
        }
        set_language(prev);
    }
}
