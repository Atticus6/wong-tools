//! 截图历史：落盘 + 记录 + 瀑布流分列。
//!
//! 全屏/区域每次落一张（50 封顶，超的老文件一起删），缩略图 480px 另存，
//! 列表走小 JSON 进库。分列是纯函数，可测。读缩略图失败的格子调用方跳过
//! （文件被外部删了不崩）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// 历史上限：再多翻着累，文件也占地方。
pub const HISTORY_MAX: usize = 50;
/// 历史目录名（挂 `pictures_dir()` 下面：`图片库/WongTools/shot-history`，
/// 用户在文件管理器里直接看得到；老版本在 `settings::data_dir()` 下的记录
/// 按绝对路径继续可用，删记录时照样清得掉）。
const HISTORY_DIR: &str = "shot-history";
/// 缩略图最大边：只做等比缩放（`thumbnail` 按原比例往 480 框里收），
/// 不裁剪不拉伸，瀑布流里看到的就是原图比例。
const THUMB_MAX: u32 = 480;

/// 一条历史：原图 + 缩略图 + 尺寸（分列估高用）+ 落盘秒。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShotRecord {
    pub id: u64,
    pub path: PathBuf,
    pub thumb: PathBuf,
    pub w: u32,
    pub h: u32,
    pub created: u64,
}

static NEXT_HISTORY_ID: AtomicU64 = AtomicU64::new(1);

/// 历史目录（不存在调用方建）。图片库优先：用户看得见、好管理。
pub fn history_dir() -> PathBuf {
    super::pictures_dir().join(HISTORY_DIR)
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 落盘一张：原图 + 缩略图 + 记录（不进库，调用方调 `push_record`）。
/// 文件名带秒 + 进程内计数，同秒连截不撞车。
pub fn save_capture(img: &image::RgbaImage) -> Result<ShotRecord, String> {
    let dir = history_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建历史目录失败: {e}"))?;
    let id = NEXT_HISTORY_ID.fetch_add(1, Ordering::Relaxed);
    let stem = format!("shot-{}-{id}", unix_secs());
    let path = dir.join(format!("{stem}.png"));
    let thumb_path = dir.join(format!("{stem}-thumb.png"));
    super::save_png(img, &path)?;
    let (tw, th) = super::fit_within(img.width(), img.height(), THUMB_MAX, THUMB_MAX);
    let thumb = image::imageops::thumbnail(img, tw, th);
    // 缩略图走内存编码再落盘（`encode_png` 有单测锁 PNG 魔数）。
    let bytes = super::encode_png(&thumb)?;
    std::fs::write(&thumb_path, bytes).map_err(|e| format!("存缩略图失败: {e}"))?;
    Ok(ShotRecord {
        id,
        path,
        thumb: thumb_path,
        w: img.width(),
        h: img.height(),
        created: unix_secs(),
    })
}

/// 进库：头部插入 + 截断（超的删文件，失败只记日志）+ 写库（失败记日志）。
pub fn push_record(rec: ShotRecord) {
    let mut list = crate::settings::shot_history();
    list.insert(0, rec);
    while list.len() > HISTORY_MAX {
        if let Some(old) = list.pop() {
            remove_files(&old);
        }
    }
    if let Err(e) = crate::settings::set_shot_history(&list) {
        eprintln!("存截图历史失败: {e}");
    }
}

/// 删除一条：出库 + 删文件（失败记日志）。
pub fn delete_record(id: u64) {
    let mut list = crate::settings::shot_history();
    if let Some(ix) = list.iter().position(|r| r.id == id) {
        let rec = list.remove(ix);
        remove_files(&rec);
        if let Err(e) = crate::settings::set_shot_history(&list) {
            eprintln!("存截图历史失败: {e}");
        }
    }
}

fn remove_files(rec: &ShotRecord) {
    if let Err(e) = std::fs::remove_file(&rec.path) {
        eprintln!("删历史原图失败: {e}");
    }
    if let Err(e) = std::fs::remove_file(&rec.thumb) {
        eprintln!("删历史缩略图失败: {e}");
    }
}

/// 读缩略图字节（渲染用）。
pub fn read_thumb(rec: &ShotRecord) -> Result<Vec<u8>, String> {
    std::fs::read(&rec.thumb).map_err(|e| format!("读缩略图失败: {e}"))
}

/// 瀑布流分列：贪心放当前最矮列（首个最小优先，稳定不闪）。
/// `heights` 和条目等长（列宽折出的显示高），返回每列的条目下标。
/// 纯函数，可测。
pub fn distribute(cols: usize, heights: &[f32]) -> Vec<Vec<usize>> {
    let cols = cols.max(1);
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); cols];
    let mut sums = vec![0f32; cols];
    for (ix, h) in heights.iter().enumerate() {
        let mut best = 0;
        for (c, s) in sums.iter().enumerate() {
            if *s < sums[best] {
                best = c;
            }
        }
        out[best].push(ix);
        sums[best] += h.max(0.);
    }
    out
}

#[cfg(test)]
mod tests {
    // 注意：这里不要 `use super::*`——它会把 gpui 自带的 `#[test]` 宏引进
    // 来，盖住 Rust 内置的 test 属性导致递归展开失败。只显式引入用到的项。
    use super::HISTORY_MAX;
    use super::distribute;
    use super::history_dir;

    #[test]
    fn distribute_balances_shortest_column_first() {
        // 3 列：0 高独占一列，剩下按矮列贪心。
        let got = distribute(3, &[10., 10., 10., 30., 5.]);
        assert_eq!(got, vec![vec![0, 3], vec![1, 4], vec![2]]);
        // 列数钳 1：全进一列。
        assert_eq!(distribute(0, &[1., 2.]), vec![vec![0, 1]]);
        // 空表：空列。
        let empty: Vec<Vec<usize>> = vec![Vec::new(), Vec::new()];
        assert_eq!(distribute(2, &[]), empty);
    }

    #[test]
    fn history_cap_is_sane() {
        // 上限别离谱：太小没用，太大拖死网格。
        assert!((10..=200).contains(&HISTORY_MAX));
    }

    #[test]
    fn history_dir_lives_under_pictures_library() {
        // 历史必须进用户图片库（可见可管理），不能再回应用数据目录。
        let dir = history_dir();
        assert_eq!(
            dir,
            super::super::pictures_dir().join("shot-history"),
            "历史目录必须挂在图片库/WongTools 下面"
        );
        assert!(
            dir.ends_with("shot-history"),
            "历史子目录名别乱改，文件管理器里靠它认"
        );
        assert!(
            super::super::pictures_dir().ends_with("WongTools"),
            "图片库应用目录名别乱改，手动保存和历史共用它"
        );
    }

    #[test]
    fn thumbnail_keeps_original_aspect_ratio() {
        // 瀑布流不改变图片比例：`thumbnail` 本身不保比例（800×400 进
        // 480×480 框出来还是 480×480），必须先 `fit_within` 算等比尺寸。
        // 这里走真实缩略链路断言：宽图还是宽图、高图还是高图。
        let wide = image::RgbaImage::from_pixel(800, 400, image::Rgba([1, 2, 3, 255]));
        let (tw, th) = super::super::fit_within(800, 400, super::THUMB_MAX, super::THUMB_MAX);
        let thumb = image::imageops::thumbnail(&wide, tw, th);
        assert!(thumb.width() > thumb.height(), "宽图缩完不能变方/变高");
        let ratio = thumb.width() as f64 / thumb.height() as f64;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "宽图比例坏了：原图 2:1，缩完 {ratio:.2}"
        );
        let tall = image::RgbaImage::from_pixel(400, 800, image::Rgba([1, 2, 3, 255]));
        let (tw, th) = super::super::fit_within(400, 800, super::THUMB_MAX, super::THUMB_MAX);
        let thumb = image::imageops::thumbnail(&tall, tw, th);
        assert!(thumb.height() > thumb.width(), "高图缩完不能变方/变宽");
        let ratio = thumb.height() as f64 / thumb.width() as f64;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "高图比例坏了：原图 1:2，缩完 1:{ratio:.2}"
        );
    }
}
