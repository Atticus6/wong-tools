//! macOS Vision 文字识别（accurate 级）。
//!
//! 为什么自己直调 Vision，不用 `native-ocr`：`native-ocr` 建请求时**没有设置**
//! `recognitionLanguages`，Vision 于是只用默认模型（英文）识别，中文会被
//! 识别成乱码或整行丢掉（实测：`截图工具设置` → `THE ATA LEM`，中文 UI 全废）。
//! 这里显式给一份语言表（简体中文 / 繁体 / 粤语 / 日 / 韩 放前面、当前
//! locale 优先，英文兜底），并跟 `supportedRecognitionLanguages` 取交集，
//! 保证在没装某语言模型的系统上也能安全降级。
//!
//! 识别是阻塞调用，调用方必须放后台线程（见 `ocr::recognize`）。

use std::ptr::null;

use objc2::rc::autoreleasepool;
use objc2::AnyThread;
use objc2_core_foundation::{CFData, CFRetained};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSArray, NSDictionary, NSLocale, NSString};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

/// 引擎接受的最大边。Vision 文档没硬上限，超大只是慢和费内存，取 4000
/// （跟 `native-ocr` 一致：大图整屏识别才走得到，选区一般用不上）。
pub const MAX_SIDE: u32 = 4000;

/// 识别用的语言优先级（靠前先用）。当前 locale 会被提到最前面。
///
/// 中日韩放前面是因为本工具默认中文界面，绝大多数截图含中文；`en-US`
/// 兜底保证纯英文/代码/URL 也能认。表里都是 Vision accurate 级长期支持的
/// 稳定 id，换系统版本也不至于整个失效（真不支持的会在取交集时被丢掉）。
const LANGUAGES: [&str; 8] = [
    "zh-Hans", "zh-Hant", "yue-Hans", "yue-Hant", "ja-JP", "ko-KR", "en-US", "en-GB",
];

/// 识别一张 RGBA8 图，返回 Vision 读出的原始文本（行用 `\n` 拼）。
/// 阻塞调用，必须放后台线程。
pub fn recognize_rgba(rgba: &[u8], width: u32, height: u32) -> Result<String, String> {
    if width == 0 || height == 0 || rgba.len() != (width as usize) * (height as usize) * 4 {
        return Err(format!(
            "图片尺寸不对：{width}x{height} 需要 {} 字节，给了 {}",
            width as usize * height as usize * 4,
            rgba.len()
        ));
    }
    let image = cg_image(rgba, width, height)?;
    autoreleasepool(|_| recognize(&image))
}

fn cg_image(rgba: &[u8], width: u32, height: u32) -> Result<CFRetained<CGImage>, String> {
    let data = CFData::from_bytes(rgba);
    let provider = CGDataProvider::with_cf_data(Some(&data)).ok_or("建像素数据源失败")?;
    let space = CGColorSpace::new_device_rgb().ok_or("建颜色空间失败")?;
    // 屏幕截图是不透明的 RGBA（alpha 恒 255），CGImage 用 premultipliedLast
    // 接即可（跟 native-ocr 同一套字节序约定：RGBA、每行不额外对齐）。
    unsafe {
        CGImage::new(
            width as usize,
            height as usize,
            8,
            32,
            width as usize * 4,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::Last.0),
            Some(&provider),
            null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
    .ok_or_else(|| "建 CGImage 失败".to_string())
}

fn recognize(image: &CGImage) -> Result<String, String> {
    let request = VNRecognizeTextRequest::new();
    // accurate：正文/小字要准，fast 只适合摄像头预览里的短词。
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    if let Some(langs) = language_array() {
        request.setRecognitionLanguages(&langs);
    }
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
            &NSDictionary::new(),
        )
    };
    let as_request: &VNRequest = &request;
    handler
        .performRequests_error(&NSArray::from_slice(&[as_request]))
        .map_err(|e| format!("识别失败：{}", e.localizedDescription()))?;
    let Some(observations) = request.results() else {
        return Ok(String::new());
    };
    // 每行取置信度最高的候选，行与行用 \n 拼（规整交给上层 `normalize`）。
    let lines: Vec<String> = observations
        .iter()
        .filter_map(|o| o.topCandidates(1).firstObject())
        .map(|c| c.string().to_string())
        .collect();
    Ok(lines.join("\n"))
}

/// 组装语言数组：当前 locale 优先 + `LANGUAGES`，再和系统支持列表取交集。
/// 取交集是防“设了系统不支持的语言 → 整个请求报错”。取不到支持列表时
/// 返回 None（不设语言，退回 Vision 默认，至少不崩）。
fn language_array() -> Option<objc2::rc::Retained<NSArray<NSString>>> {
    let supported: Vec<String> = {
        let req = VNRecognizeTextRequest::new();
        req.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        match unsafe { req.supportedRecognitionLanguagesAndReturnError() } {
            Ok(arr) => arr.iter().map(|s| s.to_string()).collect(),
            Err(_) => return None,
        }
    };
    if supported.is_empty() {
        return None;
    }

    // 当前系统语言（如 `zh`）优先顶到最前，其余按 LANGUAGES 顺序。
    let current = NSLocale::currentLocale().languageCode().to_string();
    let mut order: Vec<&str> = Vec::with_capacity(LANGUAGES.len() + 1);
    let cur_ok = supported.iter().any(|s| s == &current || s.starts_with(&format!("{current}-")));
    if cur_ok {
        // 用系统里真实存在的那一档（`zh` → `zh-Hans`/`zh-Hant`）而不是裸 `zh`。
        if let Some(match_) = supported.iter().find(|s| {
            s.as_str() == current || s.starts_with(&format!("{current}-"))
        }) {
            if let Some(pos) = LANGUAGES.iter().position(|l| *l == match_.as_str()) {
                // 已在表里就靠前放。
                let head = LANGUAGES[pos];
                order.push(head);
                order.extend(LANGUAGES.iter().copied().filter(|l| *l != head));
            } else {
                order.push(match_.as_str());
                order.extend(LANGUAGES.iter().copied());
            }
        }
    }
    if order.is_empty() {
        order.extend(LANGUAGES.iter().copied());
    }

    // 只保留系统真支持的。
    let kept: Vec<_> = order
        .into_iter()
        .filter(|l| supported.iter().any(|s| s == l))
        .collect();
    if kept.is_empty() {
        return None;
    }
    let strings: Vec<_> = kept
        .into_iter()
        .map(|l| NSString::from_str(l))
        .collect();
    Some(NSArray::from_retained_slice(&strings))
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::LANGUAGES;
    use super::MAX_SIDE;
    use super::recognize_rgba;

    #[test]
    fn max_side_matches_native_engine() {
        // 和 native-ocr 的 mac 上限保持一致，prepare 的下采样行为不变。
        assert_eq!(MAX_SIDE, 4000);
    }

    #[test]
    fn language_table_starts_with_cjk_and_keeps_english() {
        // 关键回归：中文必须在表里、英文必须兜底。Vision 默认只认英文，
        // 漏了中文就是用户看到的“乱码”。
        assert!(LANGUAGES.contains(&"zh-Hans"), "简体中文必须在语言表里");
        assert!(LANGUAGES.contains(&"zh-Hant"), "繁体中文必须在语言表里");
        assert!(LANGUAGES.contains(&"en-US"), "英文兜底必须在语言表里");
        // 第一项是简体中文：本工具默认中文界面。
        assert_eq!(LANGUAGES[0], "zh-Hans");
    }

    #[test]
    fn language_array_is_supported_subset_with_cjk_first() {
        // 组装出的语言表必须在系统支持范围内，且中文排前面、含英文。
        let Some(arr) = super::language_array() else {
            // 取不到支持列表时退化为 None 也算安全，但绝大多数系统能取到。
            return;
        };
        let langs: Vec<String> = arr.iter().map(|s| s.to_string()).collect();
        assert!(langs.contains(&"zh-Hans".to_string()), "中文没进语言表");
        assert!(langs.contains(&"en-US".to_string()), "英文没进语言表");
        // zh-Hans 应排最前（zh_CN locale 下），至少不能排到 en 之后。
        let zh = langs.iter().position(|l| l == "zh-Hans").expect("有中文");
        let en = langs.iter().position(|l| l == "en-US").expect("有英文");
        assert!(zh < en, "中文应排在英文前面（locale=zh 时）");
    }

    #[test]
    fn recognize_rejects_mismatched_byte_length() {
        // 字节数和尺寸对不上直接拒，不进引擎（避免 CGImage 越界读）。
        assert!(recognize_rgba(&[0, 0, 0], 2, 2).is_err());
        assert!(recognize_rgba(&[], 0, 0).is_err());
    }

    /// 端到端回归：中文必须认对，不能是乱码。
    ///
    /// 这正是用户报的“乱码”bug：`native-ocr` 不设 `recognitionLanguages`，
    /// Vision 默认只认英文，`截图工具设置` 会变成 `THE ATA LEM`、或中文整行
    /// 被丢成空。夹具图是一张白底中文 UI 截图（assets/tests/ocr_zh.png）。
    #[test]
    fn recognize_reads_chinese_without_garbling() {
        let bytes = include_bytes!("../../../assets/tests/ocr_zh.png");
        let img = image::load_from_memory(bytes).expect("夹具图能解码").to_rgba8();
        let text = recognize_rgba(img.as_raw(), img.width(), img.height()).expect("识别成功");
        // 只要有汉字就说明没走英文默认模型（`THE ATA LEM` 那种全 ASCII 乱码
        // 会被这里挡住）。逐字比对太脆（不同系统模型标点/字距有细微差别），
        // 判“含汉字”最稳且正是本 bug 的判据。
        let has_cjk = text.chars().any(|c| {
            ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c)
        });
        assert!(has_cjk, "识别结果里没有汉字，说明走了英文默认模型：{text:?}");
        // 英文兜底也没坏：夹具第二行含 `Ctrl+Alt+O`，拉丁字母要认得出来。
        let has_latin = text.chars().any(|c| c.is_ascii_alphabetic());
        assert!(has_latin, "拉丁字母也没认出来，英文兜底坏了：{text:?}");
    }
}