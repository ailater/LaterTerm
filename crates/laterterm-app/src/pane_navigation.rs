//! 基于调用方提供的最新可见窗格矩形计算方向导航，不依赖窗口状态。

use crate::workspace::PaneId;

/// 屏幕坐标系中的矩形：x 向右、y 向下，宽高必须为正且所有值有限。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PaneRect {
    pub(crate) id: PaneId,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Up,
    Down,
    Left,
    Right,
}

// 容忍相邻边由于布局取整产生的轻微交叠；单位与调用方矩形一致。
const EDGE_TOLERANCE: f64 = 0.5;
// 排除由浮点误差产生的近似点接触，正交方向实际重叠必须超过此值。
const OVERLAP_EPSILON: f64 = 0.001;

impl PaneRect {
    fn edges(self) -> Option<(f64, f64, f64, f64)> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
            || self.width <= 0.0
            || self.height <= 0.0
        {
            return None;
        }
        let (x, y, w, h) = (
            self.x as f64,
            self.y as f64,
            self.width as f64,
            self.height as f64,
        );
        Some((x, y, x + w, y + h))
    }
}

/// 从 `panes` 中查找 `current` 的方向邻居，仅考虑正交投影有实际重叠的窗格。
///
/// 优先选择方向上的最近边缘；距离相同则按 PaneId 数值升序，结果不依赖输入顺序。
/// 轻微边缘交叠按零距离处理，分隔线的间距不受限制。重复 ID 或无效矩形
/// 会导致该 ID 被忽略，以避免使用不明确的 bounds；当前 ID 无效时返回 None。
/// 传入的坐标必须使用同一坐标系和单位，缩放或 resize 后由调用方更新。
pub(crate) fn neighbor(
    current: PaneId,
    direction: Direction,
    panes: &[PaneRect],
) -> Option<PaneId> {
    let mut sources = panes.iter().filter(|p| p.id == current);
    let source = sources.next()?.edges()?;
    if sources.next().is_some() {
        return None;
    }
    let (sx1, sy1, sx2, sy2) = source;
    let mut best: Option<(f64, PaneId)> = None;
    for pane in panes {
        if pane.id == current || panes.iter().filter(|p| p.id == pane.id).count() != 1 {
            continue;
        }
        let Some((x1, y1, x2, y2)) = pane.edges() else {
            continue;
        };
        let (gap, overlap) = match direction {
            Direction::Right => (x1 - sx2, sy2.min(y2) - sy1.max(y1)),
            Direction::Left => (sx1 - x2, sy2.min(y2) - sy1.max(y1)),
            Direction::Down => (y1 - sy2, sx2.min(x2) - sx1.max(x1)),
            Direction::Up => (sy1 - y2, sx2.min(x2) - sx1.max(x1)),
        };
        if gap < -EDGE_TOLERANCE || overlap <= OVERLAP_EPSILON {
            continue;
        }
        let distance = gap.max(0.0);
        if best.is_none_or(|(d, id)| distance < d || (distance == d && pane.id.0 < id.0)) {
            best = Some((distance, pane.id));
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: u64, x: f32, y: f32, width: f32, height: f32) -> PaneRect {
        PaneRect {
            id: PaneId(id),
            x,
            y,
            width,
            height,
        }
    }
    fn n(id: u64, direction: Direction, panes: &[PaneRect]) -> Option<u64> {
        neighbor(PaneId(id), direction, panes).map(|id| id.0)
    }

    #[test]
    fn four_directions_and_outer_edges() {
        let p = [
            r(1, 10., 10., 10., 10.),
            r(2, 10., 0., 10., 9.),
            r(3, 10., 21., 10., 9.),
            r(4, 0., 10., 9., 10.),
            r(5, 21., 10., 9., 10.),
        ];
        for (dir, id) in [
            (Direction::Up, 2),
            (Direction::Down, 3),
            (Direction::Left, 4),
            (Direction::Right, 5),
        ] {
            assert_eq!(n(1, dir, &p), Some(id));
        }
        assert_eq!(n(2, Direction::Up, &p), None);
        assert_eq!(n(3, Direction::Down, &p), None);
        assert_eq!(n(4, Direction::Left, &p), None);
        assert_eq!(n(5, Direction::Right, &p), None);
    }

    #[test]
    fn nested_unequal_layout_uses_actual_overlap() {
        let p = [
            r(1, 0., 0., 100., 200.),
            r(2, 102., 0., 50., 60.),
            r(3, 102., 62., 50., 138.),
            r(4, 154., 62., 140., 138.),
        ];
        assert_eq!(n(1, Direction::Right, &p), Some(2));
        assert_eq!(n(3, Direction::Left, &p), Some(1));
        assert_eq!(n(2, Direction::Down, &p), Some(3));
        assert_eq!(n(4, Direction::Up, &p), None);
        assert_eq!(n(3, Direction::Right, &p), Some(4));
    }

    #[test]
    fn closest_edge_wins_not_center_or_input_order() {
        let p = [
            r(1, 0., 0., 10., 100.),
            r(2, 30., 0., 10., 100.),
            r(3, 12., 90., 200., 10.),
        ];
        assert_eq!(n(1, Direction::Right, &p), Some(3));
        assert_eq!(n(3, Direction::Left, &p), Some(1));
    }

    #[test]
    fn excludes_diagonal_and_touching_projection() {
        let p = [
            r(1, 0., 0., 10., 10.),
            r(2, 10., 10., 10., 10.),
            r(3, 11., 20., 10., 10.),
            r(4, 12., -10., 10., 10.),
        ];
        for d in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(n(1, d, &p), None);
        }
    }

    #[test]
    fn equal_distance_breaks_ties_by_id_regardless_of_order() {
        let mut p = [
            r(1, 0., 0., 10., 20.),
            r(8, 11., 0., 10., 10.),
            r(3, 11., 10., 10., 10.),
        ];
        assert_eq!(n(1, Direction::Right, &p), Some(3));
        p.reverse();
        assert_eq!(n(1, Direction::Right, &p), Some(3));
    }

    #[test]
    fn fractional_edges_separator_and_projection_tolerance() {
        let p = [
            r(1, 0., 0., 10., 10.),
            r(2, 9.75, 0., 10., 10.),
            r(3, 10.1, 9.9995, 10., 10.),
            r(4, 10.25, 9.9, 10., 10.),
        ];
        assert_eq!(n(1, Direction::Right, &p), Some(2));
        assert_eq!(n(1, Direction::Right, &[p[0], p[2]]), None);
        assert_eq!(n(1, Direction::Right, &[p[0], p[3]]), Some(4));
        assert_eq!(
            n(1, Direction::Right, &[p[0], r(5, 9.4, 0., 10., 10.)]),
            None
        );
    }

    #[test]
    fn invalid_missing_and_duplicate_ids_are_ignored() {
        let p = [
            r(1, 0., 0., 10., 10.),
            r(2, f32::NAN, 0., 1., 1.),
            r(3, 11., 0., 0., 1.),
            r(4, 11., 0., 1., f32::INFINITY),
            r(5, 11., 0., 1., 1.),
            r(5, 12., 0., 1., 1.),
            r(6, 20., 0., 1., 1.),
        ];
        assert_eq!(n(1, Direction::Right, &p), Some(6));
        assert_eq!(n(5, Direction::Right, &p), None);
        assert_eq!(n(2, Direction::Right, &p), None);
        assert_eq!(n(99, Direction::Right, &p), None);
    }

    #[test]
    fn refreshed_bounds_change_result() {
        let mut p = [
            r(1, 0., 0., 10., 10.),
            r(2, 11., 0., 10., 10.),
            r(3, 22., 0., 10., 10.),
        ];
        assert_eq!(n(1, Direction::Right, &p), Some(2));
        p[1].y = 20.;
        assert_eq!(n(1, Direction::Right, &p), Some(3));
    }
}
