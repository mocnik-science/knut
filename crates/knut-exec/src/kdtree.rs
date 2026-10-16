//! KD-tree for k-nearest-neighbour searches in a few dimensions (`classify`). Built in place
//! by median splits; ties in distance go to the smaller point index, so results are
//! deterministic.

use rayon::prelude::*;

const LEAF: usize = 8;

pub struct KdTree {
    dims: usize,
    /// Point coordinates, `dims` per point, in tree order.
    coords: Vec<f64>,
    /// Original index of each point in tree order.
    index: Vec<u32>,
}

impl KdTree {
    /// `points[i]` has `dims` coordinates.
    pub fn build(points: &[Vec<f64>], dims: usize) -> KdTree {
        let mut order: Vec<u32> = (0..points.len() as u32).collect();
        split(points, &mut order, 0, dims);
        let coords = order.iter().flat_map(|&i| points[i as usize].iter().copied()).collect();
        KdTree { dims, coords, index: order }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The `k` nearest points to `q` as (squared distance, original index), nearest first;
    /// `skip` leaves out one original index (leave-one-out).
    pub fn nearest(&self, q: &[f64], k: usize, skip: Option<u32>) -> Vec<(f64, u32)> {
        let mut best = Vec::with_capacity(k + 1);
        if k > 0 {
            self.search(0, self.index.len(), 0, q, k, skip, &mut best);
        }
        best
    }

    #[allow(clippy::too_many_arguments)]
    fn search(&self, lo: usize, hi: usize, depth: usize, q: &[f64], k: usize, skip: Option<u32>, best: &mut Vec<(f64, u32)>) {
        if hi - lo <= LEAF {
            for i in lo..hi {
                self.consider(i, q, k, skip, best);
            }
            return;
        }
        let ax = depth % self.dims;
        let m = lo + (hi - lo) / 2;
        self.consider(m, q, k, skip, best);
        let diff = q[ax] - self.coords[m * self.dims + ax];
        let (near, far) = if diff < 0.0 { ((lo, m), (m + 1, hi)) } else { ((m + 1, hi), (lo, m)) };
        self.search(near.0, near.1, depth + 1, q, k, skip, best);
        if best.len() < k || diff * diff <= best[best.len() - 1].0 {
            self.search(far.0, far.1, depth + 1, q, k, skip, best);
        }
    }

    fn consider(&self, i: usize, q: &[f64], k: usize, skip: Option<u32>, best: &mut Vec<(f64, u32)>) {
        let id = self.index[i];
        if Some(id) == skip {
            return;
        }
        let p = &self.coords[i * self.dims..(i + 1) * self.dims];
        let d: f64 = p.iter().zip(q).map(|(a, b)| (a - b) * (a - b)).sum();
        let x = (d, id);
        if best.len() < k || lt(x, best[best.len() - 1]) {
            let at = best.partition_point(|&y| lt(y, x));
            best.insert(at, x);
            best.truncate(k);
        }
    }
}

fn lt(a: (f64, u32), b: (f64, u32)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 < b.1)
}

fn split(points: &[Vec<f64>], order: &mut [u32], depth: usize, dims: usize) {
    if order.len() <= LEAF {
        return;
    }
    let ax = depth % dims;
    let m = order.len() / 2;
    order.select_nth_unstable_by(m, |&a, &b| points[a as usize][ax].total_cmp(&points[b as usize][ax]).then(a.cmp(&b)));
    let (l, r) = order.split_at_mut(m);
    let r = &mut r[1..];
    if l.len() > 50_000 {
        rayon::join(|| split(points, l, depth + 1, dims), || split(points, r, depth + 1, dims));
    } else {
        split(points, l, depth + 1, dims);
        split(points, r, depth + 1, dims);
    }
}

/// Runs `f` for every query in parallel.
pub fn par_map<T: Send, F: Fn(usize) -> T + Sync + Send>(n: usize, f: F) -> Vec<T> {
    (0..n).into_par_iter().map(f).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(points: &[Vec<f64>], q: &[f64], k: usize, skip: Option<u32>) -> Vec<(f64, u32)> {
        let mut d: Vec<(f64, u32)> = points
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i as u32) != skip)
            .map(|(i, p)| (p.iter().zip(q).map(|(a, b)| (a - b) * (a - b)).sum(), i as u32))
            .collect();
        d.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        d.truncate(k);
        d
    }

    #[test]
    fn same_neighbours_as_brute_force() {
        let mut s = 7u64;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        for dims in [1, 2, 3, 5] {
            // coarse values, so that ties occur
            let pts: Vec<Vec<f64>> = (0..3000).map(|_| (0..dims).map(|_| (rnd() * 20.0).floor()).collect()).collect();
            let t = KdTree::build(&pts, dims);
            for i in (0..pts.len()).step_by(37) {
                assert_eq!(t.nearest(&pts[i], 5, Some(i as u32)), brute(&pts, &pts[i], 5, Some(i as u32)), "dims {dims}, point {i}");
            }
        }
    }
}
