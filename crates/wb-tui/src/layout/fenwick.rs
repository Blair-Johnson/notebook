//! Binary indexed tree (`FenwickTree`) mapping visual cell indices to cumulative row heights.

#[derive(Clone, Debug, Default)]
pub struct FenwickTree {
    tree: Vec<i64>,
    heights: Vec<u32>,
}

impl FenwickTree {
    pub fn with_len(len: usize) -> Self {
        Self {
            tree: vec![0; len + 1],
            heights: vec![0; len],
        }
    }

    pub fn len(&self) -> usize {
        self.heights.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    pub fn height(&self, index: usize) -> u32 {
        self.heights[index]
    }

    pub fn set(&mut self, index: usize, height: u32) {
        let delta = i64::from(height) - i64::from(self.heights[index]);
        self.heights[index] = height;
        let mut i = index + 1;
        while i < self.tree.len() {
            self.tree[i] += delta;
            i += i & i.wrapping_neg();
        }
    }

    /// Cumulative height of cells in `0..index`.
    pub fn prefix_sum(&self, index: usize) -> u32 {
        let (mut i, mut sum) = (index.min(self.heights.len()), 0_i64);
        while i > 0 {
            sum += self.tree[i];
            i -= i & i.wrapping_neg();
        }
        sum.max(0) as u32
    }

    pub fn total_height(&self) -> u32 {
        self.prefix_sum(self.heights.len())
    }

    /// Resolve a global `scroll_row` in $O(\log N)$ time to `(cell_index, local_row)`.
    pub fn find_row(&self, scroll_row: u32) -> Option<(usize, u32)> {
        if self.heights.is_empty() {
            return None;
        }
        let (mut pos, mut rem) = (0_usize, i64::from(scroll_row));
        let mut step = self.tree.len().next_power_of_two() / 2;
        while step > 0 {
            let next = pos + step;
            if next < self.tree.len() && self.tree[next] <= rem {
                pos = next;
                rem -= self.tree[next];
            }
            step /= 2;
        }
        Some((pos.min(self.heights.len() - 1), rem.max(0) as u32))
    }
}
