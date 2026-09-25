//! 纯几何 / 路径工具。
//!
//! 这里刻意不依赖 Wayland、D-Bus 或任何 I/O，全部逻辑都可以用单元测试覆盖。
//! 坐标一律是「逻辑坐标」（合成器坐标），与物理像素的换算集中在 [`plan_crop`]。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 选区最小边长；小于它视为「只是点了一下」，不作为有效选区。
pub const MIN_SIZE: u32 = 2;

/// 判定「候选逻辑区域 ↔ 截图尺寸」比例是否一致的容差。
pub const SCALE_TOLERANCE: f64 = 0.01;

/// 逻辑坐标下的矩形（右/下边界不含）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w as i32)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h as i32)
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// 与另一个矩形求交（左闭右开），无交集时返回 `None`。
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// 包围两个矩形的最小矩形。
    pub fn union(&self, other: &Rect) -> Rect {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
    }

    /// 四周各外扩 `by` 像素。
    pub fn expand(&self, by: u32) -> Rect {
        Rect::new(
            self.x - by as i32,
            self.y - by as i32,
            self.w + by * 2,
            self.h + by * 2,
        )
    }
}

/// 把一次拖拽（两个浮点坐标，可能是任意方向）归一化成合法矩形。
///
/// - 结果一定落在 `bounds` 内；
/// - 宽或高小于 [`MIN_SIZE`] 时返回 `None`。
pub fn normalize_drag(a: (f64, f64), b: (f64, f64), bounds: (u32, u32)) -> Option<Rect> {
    let bw = bounds.0 as f64;
    let bh = bounds.1 as f64;

    let x0 = a.0.min(b.0).clamp(0.0, bw).floor();
    let x1 = a.0.max(b.0).clamp(0.0, bw).ceil();
    let y0 = a.1.min(b.1).clamp(0.0, bh).floor();
    let y1 = a.1.max(b.1).clamp(0.0, bh).ceil();

    let x = x0 as i32;
    let y = y0 as i32;
    let w = (x1 - x0).max(0.0) as u32;
    let h = (y1 - y0).max(0.0) as u32;

    // 因为 floor/ceil 的组合，理论上不会越界，这里再兜一次底。
    let w = w.min(bounds.0.saturating_sub(x.max(0) as u32));
    let h = h.min(bounds.1.saturating_sub(y.max(0) as u32));

    if w < MIN_SIZE || h < MIN_SIZE {
        return None;
    }
    Some(Rect::new(x, y, w, h))
}

/// 多个输出的逻辑矩形求并集（即「整个工作区」）。
pub fn workspace_union(rects: &[Rect]) -> Option<Rect> {
    let mut it = rects.iter().filter(|r| !r.is_empty());
    let first = *it.next()?;
    Some(it.fold(first, |acc, r| acc.union(r)))
}

/// 图像像素坐标下的裁剪矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// 由「图像尺寸 + 候选逻辑区域」推断缩放比例，并算出裁剪矩形。
///
/// `candidates` 按可信度排序，约定：
/// 1. 逻辑工作区（所有输出的并集）；
/// 2. 选区所在的输出。
///
/// 只有当某个候选区域的宽高比例与图像尺寸一致（误差 < [`SCALE_TOLERANCE`]）时才采用它；
/// 都不一致时退化为最后一个候选（选区所在输出）的比例。这样：
/// - 图像是物理像素（本机 1920×1080 vs 逻辑 1536×864，比例 1.25）时裁剪正确；
/// - 图像本身就是逻辑像素时也能正确（比例 1.0）；
/// - 多输出且缩放不同导致工作区比例失配时，退化成「按选中输出」的近似。
pub fn plan_crop(rect: Rect, img: (u32, u32), candidates: &[Rect]) -> Option<Crop> {
    if img.0 == 0 || img.1 == 0 || rect.is_empty() {
        return None;
    }
    let (region, sx, sy) = choose_region(img, candidates)?;

    let inter = rect.intersect(&region)?;
    let xf = (inter.x - region.x) as f64 * sx;
    let yf = (inter.y - region.y) as f64 * sy;
    let wf = inter.w as f64 * sx;
    let hf = inter.h as f64 * sy;

    let x = xf.round().clamp(0.0, img.0 as f64);
    let y = yf.round().clamp(0.0, img.1 as f64);
    let w = wf.round().clamp(0.0, img.0 as f64 - x);
    let h = hf.round().clamp(0.0, img.1 as f64 - y);
    if w < 1.0 || h < 1.0 {
        return None;
    }
    Some(Crop {
        x: x as u32,
        y: y as u32,
        w: w as u32,
        h: h as u32,
    })
}

/// [`plan_crop`] 实际采用的逻辑区域，供日志/自检使用。
pub fn plan_crop_region(rect: Rect, img: (u32, u32), candidates: &[Rect]) -> Option<Rect> {
    if img.0 == 0 || img.1 == 0 || rect.is_empty() {
        return None;
    }
    let region = choose_region(img, candidates)?.0;
    rect.intersect(&region).map(|_| region)
}

/// 从候选逻辑区域中挑出与图像比例一致的那个（都不一致时退化为最后一个候选）。
fn choose_region(img: (u32, u32), candidates: &[Rect]) -> Option<(Rect, f64, f64)> {
    let usable = |c: &&Rect| !c.is_empty();
    let mut chosen: Option<(Rect, f64, f64)> = None;

    for c in candidates.iter().filter(usable) {
        let sx = img.0 as f64 / c.w as f64;
        let sy = img.1 as f64 / c.h as f64;
        let uniform = (sx - sy).abs() / sx.max(sy) <= SCALE_TOLERANCE;
        if uniform {
            chosen = Some((*c, sx, sy));
            break;
        }
    }

    match chosen {
        Some(v) => Some(v),
        // 约定：候选列表最后一项是「选区所在输出」，比例算不出来时退化成它。
        None => candidates
            .iter()
            .rev()
            .find(|c| !c.is_empty())
            .map(|c| (*c, img.0 as f64 / c.w as f64, img.1 as f64 / c.h as f64)),
    }
}

/// `file:///home/u/Pictures/a%20b.png` → `/home/u/Pictures/a b.png`
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // 允许 file://host/path 形式：只取第一个 '/' 之后的部分。
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or(rest);
    let bytes = percent_decode(path);
    let s = String::from_utf8(bytes).ok()?;
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 是否允许删除 portal 为本次调用生成的临时截图。
///
/// 刻意保守：必须是普通文件、位于已知的下载/缓存/临时目录之下，且修改时间不早于
/// `created_after`（允许几秒时钟抖动）。任何一条不满足就放弃删除，宁可留下文件。
pub fn may_delete_portal_file(path: &Path, dirs: &[PathBuf], created_after: SystemTime) -> bool {
    let inside = dirs.iter().any(|d| path.starts_with(d));
    if !inside {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let slack = Duration::from_secs(5);
    match created_after.checked_sub(slack) {
        Some(floor) => mtime >= floor,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_normalizes_all_directions() {
        let bounds = (1536, 864);
        let expect = Rect::new(100, 100, 400, 300);
        // 左上 → 右下
        assert_eq!(
            normalize_drag((100.0, 100.0), (500.0, 400.0), bounds),
            Some(expect)
        );
        // 右下 → 左上
        assert_eq!(
            normalize_drag((500.0, 400.0), (100.0, 100.0), bounds),
            Some(expect)
        );
        // 右上 → 左下
        assert_eq!(
            normalize_drag((500.0, 100.0), (100.0, 400.0), bounds),
            Some(expect)
        );
        // 左下 → 右上
        assert_eq!(
            normalize_drag((100.0, 400.0), (500.0, 100.0), bounds),
            Some(expect)
        );
    }

    #[test]
    fn drag_clamps_to_bounds() {
        let bounds = (1536, 864);
        let r = normalize_drag((-50.0, -50.0), (5000.0, 5000.0), bounds).unwrap();
        assert_eq!(r, Rect::new(0, 0, 1536, 864));
    }

    #[test]
    fn drag_rejects_tiny_selection() {
        let bounds = (1536, 864);
        assert_eq!(normalize_drag((10.0, 10.0), (10.5, 10.5), bounds), None);
        assert_eq!(normalize_drag((10.0, 10.0), (11.0, 200.0), bounds), None);
    }

    #[test]
    fn drag_full_screen_rounds_outward() {
        let r = normalize_drag((0.9, 0.9), (1535.2, 863.2), (1536, 864)).unwrap();
        assert_eq!(r, Rect::new(0, 0, 1536, 864));
    }

    #[test]
    fn crop_uses_physical_scale_when_image_is_native() {
        // 本机实测：逻辑 1536×864，portal 返回 1920×1080（比例 1.25）
        let workspace = Rect::new(0, 0, 1536, 864);
        let output = workspace;
        let crop = plan_crop(
            Rect::new(0, 0, 1536, 864),
            (1920, 1080),
            &[workspace, output],
        )
        .unwrap();
        assert_eq!(
            crop,
            Crop {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            }
        );
    }

    #[test]
    fn crop_scales_sub_selection() {
        let workspace = Rect::new(0, 0, 1536, 864);
        let crop = plan_crop(
            Rect::new(100, 200, 400, 300),
            (1920, 1080),
            &[workspace, workspace],
        )
        .unwrap();
        assert_eq!(
            crop,
            Crop {
                x: 125,
                y: 250,
                w: 500,
                h: 375
            }
        );
    }

    #[test]
    fn crop_uses_one_to_one_when_image_is_logical() {
        let workspace = Rect::new(0, 0, 1536, 864);
        let crop = plan_crop(
            Rect::new(10, 20, 30, 40),
            (1536, 864),
            &[workspace, workspace],
        )
        .unwrap();
        assert_eq!(
            crop,
            Crop {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            }
        );
    }

    #[test]
    fn crop_offsets_by_output_origin() {
        // 两个输出横向排列，选区在右边那块
        let left = Rect::new(0, 0, 1536, 864);
        let right = Rect::new(1536, 0, 1920, 1080);
        let workspace = workspace_union(&[left, right]).unwrap();
        assert_eq!(workspace, Rect::new(0, 0, 3456, 1080));
        // 工作区比例 3.2 vs 图像比例(两屏原生像素拼接, 3840×1080 = 3.56) 不一致 →
        // 退化为选中输出（右屏 1920×1080 逻辑 → 原生 1.0 比例，这里给 1920×1080 图像）
        let crop = plan_crop(
            Rect::new(1636, 100, 200, 150),
            (1920, 1080),
            &[workspace, right],
        )
        .unwrap();
        assert_eq!(
            crop,
            Crop {
                x: 100,
                y: 100,
                w: 200,
                h: 150
            }
        );
    }

    #[test]
    fn crop_rejects_selection_outside_region() {
        let workspace = Rect::new(0, 0, 1536, 864);
        assert_eq!(
            plan_crop(
                Rect::new(2000, 2000, 100, 100),
                (1920, 1080),
                &[workspace, workspace]
            ),
            None
        );
    }

    #[test]
    fn file_uri_decoding() {
        assert_eq!(
            file_uri_to_path("file:///home/u/Pictures/a.png"),
            Some(PathBuf::from("/home/u/Pictures/a.png"))
        );
        assert_eq!(
            file_uri_to_path("file:///home/u/a%20b%2Bc.png"),
            Some(PathBuf::from("/home/u/a b+c.png"))
        );
        assert_eq!(file_uri_to_path("/home/u/a.png"), None);
        assert_eq!(file_uri_to_path("file://"), None);
    }

    #[test]
    fn delete_guard_requires_known_dir_and_fresh_file() {
        let dir = std::env::temp_dir();
        let p = dir.join("easy-screenshot-guard-test.txt");
        std::fs::write(&p, b"x").unwrap();
        let now = SystemTime::now();

        assert!(may_delete_portal_file(&p, std::slice::from_ref(&dir), now));
        // 不在允许目录里
        assert!(!may_delete_portal_file(
            &p,
            &[PathBuf::from("/nonexistent-dir")],
            now
        ));
        // 文件比本次运行早太多
        assert!(!may_delete_portal_file(
            &p,
            std::slice::from_ref(&dir),
            now + Duration::from_secs(600)
        ));
        // 不存在的文件
        assert!(!may_delete_portal_file(
            &dir.join("easy-screenshot-missing"),
            &[dir],
            now
        ));

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rect_set_ops() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
        assert_eq!(a.intersect(&b), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(a.intersect(&Rect::new(20, 20, 1, 1)), None);
        assert_eq!(a.expand(2), Rect::new(-2, -2, 14, 14));
    }
}
