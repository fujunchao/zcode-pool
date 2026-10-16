//! 登录窗口的容量、槽位和几何规划。只返回落位计划，不调用原生窗口接口。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub(super) const MAX_WINDOWS: usize = 5;
pub(super) const DEFAULT_WIDTH: f64 = 480.0;
pub(super) const DEFAULT_HEIGHT: f64 = 680.0;
pub(super) const MIN_WIDTH: f64 = 420.0;
pub(super) const MIN_HEIGHT: f64 = 560.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Display {
    pub name: Option<String>,
    pub area: Rect,
    pub scale: f64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Placement {
    pub outer: Rect,
    pub inner: Size,
    pub work_area: Rect,
    pub scale: f64,
    pub slot: usize,
    pub layered: bool,
}

#[derive(Clone)]
struct Layout {
    display: Display,
    inner: Size,
    outer_width: i32,
    outer_height: i32,
    x0: i32,
    y0: i32,
    gap: i32,
    cols: usize,
    rows: usize,
    dx: i32,
    dy: i32,
    layered: bool,
}

// 先保留窗口宽度，再保留间距；同等条件下优先更多列，保持原有正常布局。
// 仅收窄客户区，不为容纳额外行压缩高度，避免验证码区域过矮。
fn fit_tiled_grid(
    area: Rect,
    capacity: usize,
    width: i32,
    height: i32,
    min_width: i32,
    gap: i32,
    min_gap: i32,
) -> Option<(i32, i32, usize)> {
    (1..=capacity)
        .filter_map(|cols| {
            let rows = capacity.div_ceil(cols) as i64;
            let columns = cols as i64;
            let vertical_gap = (area.height as i64 - rows * height as i64) / (rows + 1);
            let max_gap = (gap as i64).min(vertical_gap);
            if max_gap < min_gap as i64 {
                return None;
            }
            let full_width_gap =
                ((area.width as i64 - columns * width as i64) / (columns + 1)).min(max_gap);
            if full_width_gap >= min_gap as i64 {
                return Some((width, full_width_gap as i32, cols));
            }
            let fitted_width = (area.width as i64 - (columns + 1) * min_gap as i64) / columns;
            (fitted_width >= min_width as i64).then_some((
                fitted_width.min(width as i64) as i32,
                min_gap,
                cols,
            ))
        })
        .max()
}

impl Layout {
    fn new(display: &Display, capacity: usize, decoration: Size) -> Result<Self, &'static str> {
        let area = display.area;
        let scale = display.scale;
        if !scale.is_finite()
            || scale <= 0.0
            || area.width < 160
            || area.height < 160
            || area.x.checked_add(area.width).is_none()
            || area.y.checked_add(area.height).is_none()
            || !decoration.width.is_finite()
            || !decoration.height.is_finite()
            || decoration.width < 0.0
            || decoration.height < 0.0
        {
            return Err("invalid monitor work area or scale");
        }
        let capacity = capacity.clamp(1, MAX_WINDOWS);
        let gap = ((10.0 * scale).round() as i32).clamp(1, area.width.min(area.height) / 4);
        let (aw, ah) = (area.width - 2 * gap, area.height - 2 * gap);
        let preferred_width = ((DEFAULT_WIDTH + decoration.width) * scale).ceil() as i32;
        let width = preferred_width.clamp(1, aw);
        let mut height =
            (((DEFAULT_HEIGHT + decoration.height) * scale).ceil() as i32).clamp(1, ah);
        let min_gap = ((6.0 * scale).round() as i32).clamp(1, gap);
        let min_width = ((MIN_WIDTH + decoration.width) * scale).ceil() as i32;
        let (width, gap, cols) = fit_tiled_grid(
            area,
            capacity,
            preferred_width,
            height,
            min_width,
            gap,
            min_gap,
        )
        .unwrap_or_else(|| {
            // 可读平铺方案全部失败后，才沿用原有屏内层叠布局。
            (
                width,
                gap,
                (((aw + gap) / (width + gap)) as usize).clamp(1, capacity),
            )
        });
        let (aw, ah) = (area.width - 2 * gap, area.height - 2 * gap);
        let rows_needed = capacity.div_ceil(cols);
        let rows_fit = (((ah + gap) / (height + gap)) as usize).max(1);
        let rows = rows_needed.min(rows_fit);
        let layers = capacity.div_ceil(cols * rows);
        // 屏内层叠：为后层标题栏预留空间，而不是继续向屏幕下方新增行。
        let dy = ((28.0 * scale).round() as i32).clamp(1, (ah / (layers as i32 + 1)).max(1));
        let tail = dy * (layers as i32 - 1);
        height = height.min((ah - tail - (rows as i32 - 1) * gap) / rows as i32);
        let inner = Size {
            width: width as f64 / scale - decoration.width,
            height: height as f64 / scale - decoration.height,
        };
        if inner.width < 1.0 || inner.height < 1.0 {
            return Err("monitor work area is too small for a login window");
        }
        let grid_width = cols as i32 * width + (cols as i32 - 1) * gap;
        let grid_height = rows as i32 * height + (rows as i32 - 1) * gap;
        let dx = dy.min((aw - grid_width) / (layers as i32 - 1).max(1));
        Ok(Self {
            display: display.clone(),
            inner,
            outer_width: width,
            outer_height: height,
            x0: area.x + gap + (aw - grid_width - dx * (layers as i32 - 1)) / 2,
            y0: area.y + gap + (ah - grid_height - tail) / 2,
            gap,
            cols,
            rows,
            dx,
            dy,
            layered: layers > 1,
        })
    }

    fn placement(&self, slot: usize) -> Placement {
        let layer = slot / (self.cols * self.rows);
        let cell = slot % (self.cols * self.rows);
        let area = self.display.area;
        // 手动另开窗口可能超过首次预计容量；额外槽位也必须留在工作区内。
        let x = self.x0
            + (cell % self.cols) as i32 * (self.outer_width + self.gap)
            + layer as i32 * self.dx;
        let y = self.y0
            + (cell / self.cols) as i32 * (self.outer_height + self.gap)
            + layer as i32 * self.dy;
        Placement {
            outer: Rect {
                x: x.clamp(area.x, area.x + area.width - self.outer_width),
                y: y.clamp(area.y, area.y + area.height - self.outer_height),
                width: self.outer_width,
                height: self.outer_height,
            },
            inner: self.inner,
            work_area: area,
            scale: self.display.scale,
            slot,
            layered: self.layered || layer > 0,
        }
    }
}

#[derive(Default)]
struct State {
    // None 表示已预占、仍在初始化的窗口；不能被重排或失效清理误删。
    members: BTreeMap<String, Option<usize>>,
    capacity: usize,
    layout: Option<Layout>,
}

#[derive(Clone, Default)]
pub(super) struct Registry(Arc<Mutex<State>>);

impl Registry {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn reserve(&self, label: &str, capacity: usize) -> Option<Permit> {
        let mut state = self.state();
        if state.members.len() >= MAX_WINDOWS || state.members.contains_key(label) {
            return None;
        }
        state.members.insert(label.to_string(), None);
        state.capacity = state
            .capacity
            .max(capacity.clamp(1, MAX_WINDOWS))
            .max(state.members.len());
        Some(Permit {
            registry: self.clone(),
            label: label.to_string(),
            released: AtomicBool::new(false),
        })
    }

    pub fn placed_labels(&self) -> Vec<String> {
        self.state()
            .members
            .iter()
            .filter(|(_, slot)| slot.is_some())
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn focus_candidates(&self, closing: &str) -> Vec<String> {
        let state = self.state();
        let Some(Some(source_slot)) = state.members.get(closing) else {
            return Vec::new();
        };
        let mut candidates: Vec<_> = state
            .members
            .iter()
            .filter_map(|(label, slot)| {
                let slot = (*slot)?;
                (label != closing).then(|| (slot, label.clone()))
            })
            .collect();
        // 从当前槽位往后寻找，末尾回到开头；不依赖随机流程标识的字典序。
        candidates.sort_by_key(|(slot, _)| (*slot <= *source_slot, *slot));
        candidates.into_iter().map(|(_, label)| label).collect()
    }

    pub fn place(
        &self,
        label: &str,
        displays: &[Display],
        preferred: &Display,
        decoration: Size,
    ) -> Result<Placement, &'static str> {
        let mut state = self.state();
        let previous = *state
            .members
            .get(label)
            .ok_or("login window reservation has expired")?;
        // 一次会话优先沿用原屏幕；工作区/缩放变化或显示器移除后，仅为新窗口重算。
        if !state
            .layout
            .as_ref()
            .is_some_and(|layout| displays.contains(&layout.display))
        {
            state.layout = Some(Layout::new(preferred, state.capacity, decoration)?);
        }
        let used: BTreeSet<usize> = state.members.values().flatten().copied().collect();
        let slot =
            previous.unwrap_or_else(|| (0..MAX_WINDOWS).find(|s| !used.contains(s)).unwrap());
        state.members.insert(label.to_string(), Some(slot));
        Ok(state.layout.as_ref().unwrap().placement(slot))
    }

    pub fn arrange(
        &self,
        labels: &[String],
        display: &Display,
        decoration: Size,
    ) -> Result<Vec<(String, Placement)>, &'static str> {
        let mut state = self.state();
        let layout = Layout::new(display, state.capacity, decoration)?;
        // 原生适配层在主线程取得已落位且仍存活的窗口；在途预占不受清理影响。
        state
            .members
            .retain(|label, slot| slot.is_none() || labels.contains(label));
        let mut ordered: Vec<_> = labels
            .iter()
            .filter_map(|label| {
                state
                    .members
                    .get(label)
                    .copied()
                    .flatten()
                    .map(|slot| (slot, label.clone()))
            })
            .collect();
        ordered.sort();
        let placements = ordered
            .into_iter()
            .enumerate()
            .map(|(slot, (_, label))| {
                state.members.insert(label.clone(), Some(slot));
                (label, layout.placement(slot))
            })
            .collect();
        state.layout = Some(layout);
        Ok(placements)
    }
}

/// 名额从异步初始化前一直持有至 Destroyed；失败返回和异常路径由 Drop 归还。
pub(crate) struct Permit {
    registry: Registry,
    label: String,
    released: AtomicBool,
}

impl Permit {
    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.registry.state();
        state.members.remove(&self.label);
        if state.members.is_empty() {
            state.layout = None;
            state.capacity = 0;
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECORATION: Size = Size {
        width: 20.0,
        height: 54.0,
    };

    fn display(width: i32, height: i32, scale: f64) -> Display {
        Display {
            name: Some("test".into()),
            area: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            scale,
        }
    }

    fn inside(area: Rect, rect: Rect) -> bool {
        rect.width > 0
            && rect.height > 0
            && rect.x >= area.x
            && rect.y >= area.y
            && rect.x + rect.width <= area.x + area.width
            && rect.y + rect.height <= area.y + area.height
    }

    fn place(registry: &Registry, label: &str, screen: &Display) -> Placement {
        registry
            .place(label, &[screen.clone()], screen, DECORATION)
            .unwrap()
    }

    fn placements(screen: &Display, count: usize) -> Vec<Placement> {
        let registry = Registry::default();
        let mut permits = Vec::new();
        (0..count)
            .map(|n| {
                let label = n.to_string();
                permits.push(registry.reserve(&label, count).unwrap());
                place(&registry, &label, screen)
            })
            .collect()
    }

    fn assert_tiled(screen: &Display, placements: &[Placement]) {
        for (index, placement) in placements.iter().enumerate() {
            assert!(
                inside(screen.area, placement.outer),
                "窗口越界：{placement:?}"
            );
            for other in &placements[index + 1..] {
                let (a, b) = (placement.outer, other.outer);
                assert!(
                    a.x + a.width <= b.x
                        || b.x + b.width <= a.x
                        || a.y + a.height <= b.y
                        || b.y + b.height <= a.y,
                    "可平铺的窗口发生遮挡：{a:?} / {b:?}，工作区={screen:?}"
                );
            }
            assert!(!placement.layered, "能够平铺时不应启用层叠：{placement:?}");
        }
    }

    #[test]
    fn three_windows_on_1080p_at_125_percent_do_not_overlap() {
        let screen = display(1920, 1030, 1.25);
        let windows = placements(&screen, 3);
        assert_tiled(&screen, &windows);
        // 只收紧间距就能容纳三列，不应先缩小客户区。
        assert!(windows.iter().all(|p| p.inner.width == DEFAULT_WIDTH));
        assert!(windows.iter().all(|p| p.outer.y == windows[0].outer.y));
        assert_eq!(
            windows.iter().map(|p| p.outer.x).collect::<Vec<_>>(),
            vec![11, 647, 1283]
        );
    }

    #[test]
    fn two_windows_keep_the_existing_default_layout() {
        let screen = display(1920, 1030, 1.25);
        let windows = placements(&screen, 2);
        assert_tiled(&screen, &windows);
        assert_eq!(
            windows[0].outer,
            Rect {
                x: 328,
                y: 56,
                width: 625,
                height: 918
            }
        );
        assert_eq!(
            windows[1].outer,
            Rect {
                x: 966,
                y: 56,
                width: 625,
                height: 918
            }
        );
    }

    #[test]
    fn three_windows_narrow_before_layering_on_a_small_screen() {
        let screen = display(1366, 720, 1.0);
        let windows = placements(&screen, 3);
        assert_tiled(&screen, &windows);
        assert!(windows
            .iter()
            .all(|p| (420.0..DEFAULT_WIDTH).contains(&p.inner.width)));
        assert!(windows.iter().all(|p| p.outer.y == windows[0].outer.y));
    }

    #[test]
    fn two_rows_tighten_gaps_without_shrinking_window_height() {
        let screen = display(1030, 1490, 1.0);
        let windows = placements(&screen, 3);
        assert_tiled(&screen, &windows);
        assert!(windows
            .iter()
            .all(|p| p.inner.width == DEFAULT_WIDTH && p.inner.height == DEFAULT_HEIGHT));
        assert!(windows[2].outer.y > windows[0].outer.y);
    }

    #[test]
    fn minimum_width_boundary_includes_gaps_and_screen_margins() {
        // 三扇最小客户区 420 + 边框 20，四处最小间距 6，共需 1344 像素。
        let screen = display(1344, 720, 1.0);
        let windows = placements(&screen, 3);
        assert_tiled(&screen, &windows);
        assert!(windows.iter().all(|p| p.inner.width >= 420.0));

        let too_narrow = display(1343, 720, 1.0);
        let layered = placements(&too_narrow, 3);
        assert!(layered
            .iter()
            .all(|p| p.layered && inside(too_narrow.area, p.outer)));
    }

    #[test]
    fn readable_grid_matrix_does_not_fall_back_to_layering() {
        for (width, height) in [
            (800, 560),
            (1366, 720),
            (1920, 1030),
            (2560, 1400),
            (3840, 2120),
        ] {
            for scale in [1.0_f64, 1.25, 1.5, 1.75, 2.0] {
                for count in 1_usize..=5 {
                    let mut screen = display(width, height, scale);
                    screen.area.x = -width;
                    screen.area.y = 40;
                    let windows = placements(&screen, count);
                    let gap = (6.0 * scale).round() as i32;
                    let default_gap = (10.0 * scale).round() as i32;
                    let min_width = ((420.0 + DECORATION.width) * scale).ceil() as i32;
                    let outer_height = (((DEFAULT_HEIGHT + DECORATION.height) * scale).ceil()
                        as i32)
                        .min(height - 2 * default_gap);
                    // 根据可读尺寸判断是否存在网格，不复制规划器的选优过程。
                    let fits = (1..=count).any(|cols| {
                        let rows = count.div_ceil(cols) as i32;
                        let cols = cols as i32;
                        cols * min_width + (cols + 1) * gap <= width
                            && rows * outer_height + (rows + 1) * gap <= height
                    });
                    if fits {
                        assert_tiled(&screen, &windows);
                        assert!(windows.iter().all(|p| p.inner.width >= 420.0));
                    } else {
                        assert!(windows.iter().all(|p| inside(screen.area, p.outer)));
                    }
                }
            }
        }
    }

    #[test]
    fn adaptive_three_window_layout_survives_refill_and_arrange() {
        let registry = Registry::default();
        let screen = display(1920, 1030, 1.25);
        let a = registry.reserve("a", 3).unwrap();
        let _b = registry.reserve("b", 3).unwrap();
        let _c = registry.reserve("c", 3).unwrap();
        let before = [
            place(&registry, "a", &screen),
            place(&registry, "b", &screen),
            place(&registry, "c", &screen),
        ];
        assert_tiled(&screen, &before);
        a.release();
        let _d = registry.reserve("d", 3).unwrap();
        assert_eq!(place(&registry, "d", &screen).outer, before[0].outer);
        assert_eq!(place(&registry, "b", &screen).outer, before[1].outer);
        assert_eq!(place(&registry, "c", &screen).outer, before[2].outer);

        let arranged = registry
            .arrange(&["c".into(), "d".into(), "b".into()], &screen, DECORATION)
            .unwrap();
        let windows: Vec<_> = arranged.iter().map(|(_, p)| *p).collect();
        assert_tiled(&screen, &windows);
        assert_eq!(
            windows.iter().map(|p| p.outer).collect::<Vec<_>>(),
            before.iter().map(|p| p.outer).collect::<Vec<_>>()
        );
    }

    #[test]
    fn focus_candidates_follow_slots_and_exclude_inflight_members() {
        let registry = Registry::default();
        let screen = display(1920, 1030, 1.25);
        let _a = registry.reserve("z-first", 3).unwrap();
        let _b = registry.reserve("b-second", 3).unwrap();
        let _c = registry.reserve("a-third", 3).unwrap();
        let _pending = registry.reserve("pending", 4).unwrap();
        for label in ["z-first", "b-second", "a-third"] {
            place(&registry, label, &screen);
        }
        assert_eq!(
            registry.focus_candidates("b-second"),
            ["a-third", "z-first"]
        );
        assert_eq!(
            registry.focus_candidates("a-third"),
            ["z-first", "b-second"]
        );
        assert!(registry.focus_candidates("pending").is_empty());
    }

    #[test]
    fn focus_candidates_use_replacement_slots_and_ignore_released_sources() {
        let registry = Registry::default();
        let screen = display(1920, 1030, 1.25);
        let a = registry.reserve("a", 3).unwrap();
        let _b = registry.reserve("b", 3).unwrap();
        let _c = registry.reserve("c", 3).unwrap();
        for label in ["a", "b", "c"] {
            place(&registry, label, &screen);
        }
        a.release();
        let _replacement = registry.reserve("replacement", 3).unwrap();
        place(&registry, "replacement", &screen);
        assert_eq!(registry.focus_candidates("c"), ["replacement", "b"]);
        assert!(registry.focus_candidates("a").is_empty());
    }

    #[test]
    fn five_login_windows_stay_inside_1080p_work_area() {
        let registry = Registry::default();
        let screen = display(1920, 1040, 1.0);
        let permits: Vec<_> = (0..5)
            .map(|n| registry.reserve(&n.to_string(), 5).unwrap())
            .collect();
        let mut positions = BTreeSet::new();
        for n in 0..5 {
            let p = place(&registry, &n.to_string(), &screen);
            assert!(inside(screen.area, p.outer), "槽位 {n} 越出工作区：{p:?}");
            assert!(
                positions.insert((p.outer.x, p.outer.y)),
                "层叠窗口应有独立的标题位置"
            );
        }
        drop(permits);
    }

    #[test]
    fn work_area_and_scale_matrix_stays_visible() {
        for (width, height) in [
            (800, 560),
            (1366, 720),
            (1920, 1040),
            (2560, 1400),
            (3840, 2120),
        ] {
            for scale in [1.0, 1.25, 1.5, 2.0] {
                for count in 1..=5 {
                    let registry = Registry::default();
                    let mut screen = display(width, height, scale);
                    screen.area.x = -width;
                    screen.area.y = 40; // 顶部任务栏/副屏负坐标。
                    let permits: Vec<_> = (0..count)
                        .map(|n| registry.reserve(&n.to_string(), count).unwrap())
                        .collect();
                    for n in 0..count {
                        let p = place(&registry, &n.to_string(), &screen);
                        assert!(
                            inside(screen.area, p.outer),
                            "{width}×{height} @{scale} / {count}: {p:?}"
                        );
                    }
                    drop(permits);
                }
            }
        }
    }

    #[test]
    fn released_slot_is_reused_without_changing_other_placements() {
        let registry = Registry::default();
        let screen = display(1920, 1040, 1.0);
        let a = registry.reserve("a", 3).unwrap();
        let _b = registry.reserve("b", 3).unwrap();
        let _c = registry.reserve("c", 3).unwrap();
        let pa = place(&registry, "a", &screen);
        let pb = place(&registry, "b", &screen);
        let pc = place(&registry, "c", &screen);
        a.release();
        let _d = registry.reserve("d", 3).unwrap();
        assert_eq!(place(&registry, "d", &screen).outer, pa.outer);
        assert_eq!(place(&registry, "b", &screen).outer, pb.outer);
        assert_eq!(place(&registry, "c", &screen).outer, pc.outer);
    }

    #[test]
    fn arrange_retains_capacity_and_inflight_reservations() {
        let registry = Registry::default();
        let screen = display(1920, 1040, 1.0);
        let _a = registry.reserve("a", 3).unwrap();
        let _b = registry.reserve("b", 3).unwrap();
        let _c = registry.reserve("c", 3).unwrap(); // 初始化中，尚未落位。
        place(&registry, "a", &screen);
        place(&registry, "b", &screen);
        let arranged = registry
            .arrange(&["b".into(), "a".into()], &screen, DECORATION)
            .unwrap();
        let c = place(&registry, "c", &screen);
        assert_eq!(arranged[0].0, "a");
        assert_eq!(
            c.outer.y, arranged[0].1.outer.y,
            "第三扇应继续使用同一行的预留槽位"
        );
        assert!(c.outer.x >= arranged[1].1.outer.x + arranged[1].1.outer.width);
        assert_eq!(
            place(&registry, "c", &screen).outer,
            c.outer,
            "重复落位不能再占一个槽位"
        );
    }

    #[test]
    fn changed_work_area_or_scale_invalidates_future_placements() {
        let registry = Registry::default();
        let old = display(1920, 1040, 1.0);
        let _a = registry.reserve("a", 2).unwrap();
        place(&registry, "a", &old);
        let _b = registry.reserve("b", 2).unwrap();
        let mut current = display(2560, 1400, 1.5);
        current.area.x = -2560;
        let p = place(&registry, "b", &current);
        assert!(inside(current.area, p.outer));
    }

    #[test]
    fn reservations_enforce_limit_across_threads_before_windows_exist() {
        use std::sync::{atomic::AtomicUsize, Barrier};
        let registry = Registry::default();
        let gate = Arc::new(Barrier::new(17));
        let count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for n in 0..16 {
                let (registry, gate, count) = (registry.clone(), gate.clone(), &count);
                scope.spawn(move || {
                    gate.wait();
                    let permit = registry.reserve(&n.to_string(), 5);
                    if permit.is_some() {
                        count.fetch_add(1, Ordering::SeqCst);
                    }
                    gate.wait(); // 所有请求完成预占后再释放，避免把复用名额计为同时占用。
                    drop(permit);
                });
            }
            gate.wait();
            gate.wait();
        });
        assert_eq!(count.load(Ordering::SeqCst), 5);
        assert!(registry.reserve("next", 1).is_some());
    }

    #[test]
    fn failed_initialization_and_duplicate_release_do_not_leak_or_steal_slots() {
        let registry = Registry::default();
        let first = registry.reserve("a", 1).unwrap();
        assert!(registry.reserve("a", 1).is_none());
        first.release();
        let second = registry.reserve("a", 5).unwrap();
        drop(first);
        assert!(
            registry.reserve("a", 1).is_none(),
            "旧凭证二次释放不能删除新的预占"
        );
        drop(second);
        assert!(registry.reserve("a", 1).is_some());
    }
}
