//! Reusable maximum-weight matching scratch, including the small top-column
//! search used by the website for at most four remaining snap slots.
#[derive(Default)]
pub struct Scratch {
    u: Vec<f64>,
    v: Vec<f64>,
    p: Vec<usize>,
    way: Vec<usize>,
    minimum: Vec<f64>,
    used: Vec<bool>,
    small_columns: [i32; 25],
    small_values: [f64; 25],
    small_used: Vec<bool>,
}
impl Scratch {
    pub fn maximum(
        &mut self,
        weights: &[Vec<f64>],
        columns: usize,
        assignment: Option<&mut [usize]>,
    ) -> f64 {
        let n = weights.len();
        if n == 0 {
            return 0.0;
        }
        if columns < n || weights.iter().any(|row| row.len() < columns) {
            return f64::NEG_INFINITY;
        }
        if weights
            .iter()
            .any(|row| row[..columns].iter().any(|v| !v.is_finite()))
        {
            return if assignment.is_some() {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        if assignment.is_none() && n <= 4 && columns <= 4096 {
            return self.small(weights, columns);
        }
        let size = n.max(columns) + 1;
        if self.u.len() < size {
            let size = size * 2;
            self.u.resize(size, 0.0);
            self.v.resize(size, 0.0);
            self.p.resize(size, 0);
            self.way.resize(size, 0);
            self.minimum.resize(size, 0.0);
            self.used.resize(size, false);
        }
        self.u[..=n].fill(0.0);
        self.v[..=columns].fill(0.0);
        self.p[..=columns].fill(0);
        self.way[..=columns].fill(0);
        let inf = f64::MAX / 4.0;
        for i in 1..=n {
            self.p[0] = i;
            let mut j0 = 0;
            self.minimum[..=columns].fill(inf);
            self.used[..=columns].fill(false);
            loop {
                self.used[j0] = true;
                let i0 = self.p[j0];
                let (mut delta, mut j1) = (inf, 0);
                for j in 1..=columns {
                    if !self.used[j] {
                        let cur = -weights[i0 - 1][j - 1] - self.u[i0] - self.v[j];
                        if cur < self.minimum[j] {
                            self.minimum[j] = cur;
                            self.way[j] = j0;
                        }
                        if self.minimum[j] < delta {
                            delta = self.minimum[j];
                            j1 = j;
                        }
                    }
                }
                for j in 0..=columns {
                    if self.used[j] {
                        self.u[self.p[j]] += delta;
                        self.v[j] -= delta;
                    } else {
                        self.minimum[j] -= delta;
                    }
                }
                j0 = j1;
                if self.p[j0] == 0 {
                    break;
                }
            }
            loop {
                let j1 = self.way[j0];
                self.p[j0] = self.p[j1];
                j0 = j1;
                if j0 == 0 {
                    break;
                }
            }
        }
        let mut assignment = assignment;
        let mut total = 0.0;
        for j in 1..=columns {
            if self.p[j] > 0 {
                total += weights[self.p[j] - 1][j - 1];
                if let Some(out) = assignment.as_deref_mut() {
                    out[self.p[j] - 1] = j - 1;
                }
            }
        }
        total
    }
    fn small(&mut self, weights: &[Vec<f64>], columns: usize) -> f64 {
        let n = weights.len();
        if self.small_used.len() < 4096 {
            self.small_used.resize(4096, false);
        }
        for (r, row) in weights.iter().enumerate() {
            let base = r * 5;
            self.small_columns[base..base + n].fill(-1);
            self.small_values[base..base + n].fill(f64::NEG_INFINITY);
            for (c, &value) in row.iter().enumerate().take(columns) {
                if value <= self.small_values[base + n - 1] {
                    continue;
                }
                let mut k = n - 1;
                while k > 0 && self.small_values[base + k - 1] < value {
                    self.small_values[base + k] = self.small_values[base + k - 1];
                    self.small_columns[base + k] = self.small_columns[base + k - 1];
                    k -= 1;
                }
                self.small_values[base + k] = value;
                self.small_columns[base + k] = c as i32;
            }
        }
        fn visit(s: &mut Scratch, n: usize, r: usize, total: f64, best: &mut f64) {
            if r == n {
                if total > *best {
                    *best = total;
                }
                return;
            }
            let base = r * 5;
            for k in 0..n {
                let c = s.small_columns[base + k];
                if c < 0 || s.small_used[c as usize] {
                    continue;
                }
                s.small_used[c as usize] = true;
                visit(s, n, r + 1, total + s.small_values[base + k], best);
                s.small_used[c as usize] = false;
            }
        }
        let mut best = f64::NEG_INFINITY;
        visit(self, n, 0, 0.0, &mut best);
        best
    }
}
