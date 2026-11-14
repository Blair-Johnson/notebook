//! Vim-style scroll margins (`scrolloff`): the view moves only when the
//! cursor comes within the margin of its top or bottom edge, so there is
//! always context on both sides, until the start or end of the content.

/// Rows to keep between the cursor and each edge of a view `height` rows
/// tall: at least `rows`, and at least `pct` percent of the height, and
/// never so many that the cursor could not move.
pub fn margin(height: usize, rows: u16, pct: u16) -> usize {
    let from_pct = (height * usize::from(pct) + 50) / 100;
    usize::from(rows)
        .max(from_pct)
        .min(height.saturating_sub(1) / 2)
}

/// The first visible row of a list of `len` rows shown `height` at a time,
/// after the cursor moved to `cursor`: `top` unless the cursor came within
/// `margin` rows of an edge.
pub fn follow(top: usize, cursor: usize, len: usize, height: usize, margin: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    let margin = margin.min((height - 1) / 2);
    let max_top = len - height;
    let mut top = top.min(max_top);
    if cursor < top + margin {
        top = cursor.saturating_sub(margin);
    } else if cursor + margin >= top + height {
        top = cursor + margin + 1 - height;
    }
    top.min(max_top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_moves_only_inside_the_margin() {
        // 100 rows, 20 visible, 2 rows of margin.
        let mut top = 0;
        for cursor in 0..=17 {
            top = follow(top, cursor, 100, 20, 2);
            assert_eq!(top, 0, "cursor {cursor} is clear of the bottom margin");
        }
        top = follow(top, 18, 100, 20, 2);
        assert_eq!(top, 1, "two rows stay below the cursor");
        top = follow(top, 99, 100, 20, 2);
        assert_eq!(top, 80, "the end of the list pins the view");
        top = follow(top, 82, 100, 20, 2);
        assert_eq!(top, 80, "moving up inside the view does not scroll");
        top = follow(top, 81, 100, 20, 2);
        assert_eq!(top, 79, "two rows stay above the cursor");
        assert_eq!(follow(40, 3, 10, 20, 2), 0, "a list that fits never scrolls");
    }

    #[test]
    fn margins_combine_rows_and_percent_and_stay_usable() {
        assert_eq!(margin(50, 2, 10), 5);
        assert_eq!(margin(17, 2, 10), 2);
        assert_eq!(margin(50, 8, 10), 8);
        assert_eq!(margin(5, 0, 50), 2, "never more than half the view");
        assert_eq!(margin(1, 3, 0), 0);
    }
}
