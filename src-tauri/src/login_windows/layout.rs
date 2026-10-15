//! 登录窗口的容量、槽位和几何规划。只返回落位计划，不调用原生窗口接口。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub(super) const MAX_WINDOWS: usize = 5;
pub(super) const DEFAULT_WIDTH: f64 = 480.0;
pub(super) const DEFAULT_HEIGHT: f64 = 680.0;

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

impl Layout {
    fn new(display: &Display, capacity: usize, decoration: Size) -> Result<Self, &'static str> {
        let area = display.area;
        let scale = display.scale;
        if !scale.is_finite() || scale <= 0.0 || area.width < 160 || area.height < 160
            || area.x.checked_add(area.width).is_none() || area.y.checked_add(area.height).is_none()
            || !decoration.width.is_finite() || !decoration.height.is_finite()
            || decoration.width < 0.0 || decoration.height < 0.0
        {
            return Err("invalid monitor work area or scale");
        }
        let capacity = capacity.clamp(1, MAX_WINDOWS);
        let gap = ((10.0 * scale).round() as i32).clamp(1, area.width.min(area.height) / 4);
        let (aw, ah) = (area.width - 2 * gap, area.height - 2 * gap);
        let width = (((DEFAULT_WIDTH + decoration.width) * scale).ceil() as i32).clamp(1, aw);
        let mut height = (((DEFAULT_HEIGHT + decoration.height) * scale).ceil() as i32).clamp(1, ah);
        let cols = (((aw + gap) / (width + gap)) as usize).clamp(1, capacity);
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
            display: display.clone(), inner, outer_width: width, outer_height: height,
            x0: area.x + gap + (aw - grid_width - dx * (layers as i32 - 1)) / 2,
            y0: area.y + gap + (ah - grid_height - tail) / 2,
            gap, cols, rows, dx, dy, layered: layers > 1,
        })
    }

    fn placement(&self, slot: usize) -> Placement {
        let layer = slot / (self.cols * self.rows);
        let cell = slot % (self.cols * self.rows);
        let area = self.display.area;
        // 手动另开窗口可能超过首次预计容量；额外槽位也必须留在工作区内。
        let x = self.x0 + (cell % self.cols) as i32 * (self.outer_width + self.gap) + layer as i32 * self.dx;
        let y = self.y0 + (cell / self.cols) as i32 * (self.outer_height + self.gap) + layer as i32 * self.dy;
        Placement {
            outer: Rect {
                x: x.clamp(area.x, area.x + area.width - self.outer_width),
                y: y.clamp(area.y, area.y + area.height - self.outer_height),
                width: self.outer_width, height: self.outer_height,
            },
            inner: self.inner, work_area: area, slot, layered: self.layered || layer > 0,
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
        state.capacity = state.capacity.max(capacity.clamp(1, MAX_WINDOWS)).max(state.members.len());
        Some(Permit { registry: self.clone(), label: label.to_string(), released: AtomicBool::new(false) })
    }

    pub fn placed_labels(&self) -> Vec<String> {
        self.state().members.iter().filter(|(_, slot)| slot.is_some()).map(|(k, _)| k.clone()).collect()
    }

    pub fn place(&self, label: &str, displays: &[Display], preferred: &Display, decoration: Size) -> Result<Placement, &'static str> {
        let mut state = self.state();
        let previous = *state.members.get(label).ok_or("login window reservation has expired")?;
        // 一次会话优先沿用原屏幕；工作区/缩放变化或显示器移除后，仅为新窗口重算。
        if !state.layout.as_ref().is_some_and(|layout| displays.contains(&layout.display)) {
            state.layout = Some(Layout::new(preferred, state.capacity, decoration)?);
        }
        let used: BTreeSet<usize> = state.members.values().flatten().copied().collect();
        let slot = previous.unwrap_or_else(|| (0..MAX_WINDOWS).find(|s| !used.contains(s)).unwrap());
        state.members.insert(label.to_string(), Some(slot));
        Ok(state.layout.as_ref().unwrap().placement(slot))
    }

    pub fn arrange(&self, labels: &[String], display: &Display, decoration: Size) -> Result<Vec<(String, Placement)>, &'static str> {
        let mut state = self.state();
        let layout = Layout::new(display, state.capacity, decoration)?;
        // 原生适配层在主线程取得已落位且仍存活的窗口；在途预占不受清理影响。
        state.members.retain(|label, slot| slot.is_none() || labels.contains(label));
        let mut ordered: Vec<_> = labels.iter()
            .filter_map(|label| state.members.get(label).copied().flatten().map(|slot| (slot, label.clone())))
            .collect();
        ordered.sort();
        let placements = ordered.into_iter().enumerate().map(|(slot, (_, label))| {
            state.members.insert(label.clone(), Some(slot));
            (label, layout.placement(slot))
        }).collect();
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
        if self.released.swap(true, Ordering::AcqRel) { return; }
        let mut state = self.registry.state();
        state.members.remove(&self.label);
        if state.members.is_empty() {
            state.layout = None;
            state.capacity = 0;
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) { self.release(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECORATION: Size = Size { width: 20.0, height: 54.0 };

    fn display(width: i32, height: i32, scale: f64) -> Display {
        Display { name: Some("test".into()), area: Rect { x: 0, y: 0, width, height }, scale }
    }

    fn inside(area: Rect, rect: Rect) -> bool {
        rect.width > 0 && rect.height > 0 && rect.x >= area.x && rect.y >= area.y
            && rect.x + rect.width <= area.x + area.width && rect.y + rect.height <= area.y + area.height
    }

    fn place(registry: &Registry, label: &str, screen: &Display) -> Placement {
        registry.place(label, &[screen.clone()], screen, DECORATION).unwrap()
    }

    #[test]
    fn five_login_windows_stay_inside_1080p_work_area() {
        let registry = Registry::default();
        let screen = display(1920, 1040, 1.0);
        let permits: Vec<_> = (0..5).map(|n| registry.reserve(&n.to_string(), 5).unwrap()).collect();
        let mut positions = BTreeSet::new();
        for n in 0..5 {
            let p = place(&registry, &n.to_string(), &screen);
            assert!(inside(screen.area, p.outer), "槽位 {n} 越出工作区：{p:?}");
            assert!(positions.insert((p.outer.x, p.outer.y)), "层叠窗口应有独立的标题位置");
        }
        drop(permits);
    }

    #[test]
    fn work_area_and_scale_matrix_stays_visible() {
        for (width, height) in [(800, 560), (1366, 720), (1920, 1040), (2560, 1400), (3840, 2120)] {
            for scale in [1.0, 1.25, 1.5, 2.0] {
                for count in 1..=5 {
                    let registry = Registry::default();
                    let mut screen = display(width, height, scale);
                    screen.area.x = -width;
                    screen.area.y = 40; // 顶部任务栏/副屏负坐标。
                    let permits: Vec<_> = (0..count).map(|n| registry.reserve(&n.to_string(), count).unwrap()).collect();
                    for n in 0..count {
                        let p = place(&registry, &n.to_string(), &screen);
                        assert!(inside(screen.area, p.outer), "{width}×{height} @{scale} / {count}: {p:?}");
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
        let arranged = registry.arrange(&["b".into(), "a".into()], &screen, DECORATION).unwrap();
        let c = place(&registry, "c", &screen);
        assert_eq!(arranged[0].0, "a");
        assert_eq!(c.outer.y, arranged[0].1.outer.y, "第三扇应继续使用同一行的预留槽位");
        assert!(c.outer.x >= arranged[1].1.outer.x + arranged[1].1.outer.width);
        assert_eq!(place(&registry, "c", &screen).outer, c.outer, "重复落位不能再占一个槽位");
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
        use std::sync::{Barrier, atomic::AtomicUsize};
        let registry = Registry::default();
        let gate = Arc::new(Barrier::new(17));
        let count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for n in 0..16 {
                let (registry, gate, count) = (registry.clone(), gate.clone(), &count);
                scope.spawn(move || {
                    gate.wait();
                    let permit = registry.reserve(&n.to_string(), 5);
                    if permit.is_some() { count.fetch_add(1, Ordering::SeqCst); }
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
        assert!(registry.reserve("a", 1).is_none(), "旧凭证二次释放不能删除新的预占");
        drop(second);
        assert!(registry.reserve("a", 1).is_some());
    }
}
