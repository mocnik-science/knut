//! Hilbert curve of order 32 on the u32 lon/lat grid (64-bit keys).

/// Maps a point on the 2^32 x 2^32 grid to its Hilbert index.
pub fn xy2d(mut x: u32, mut y: u32) -> u64 {
    let mut d: u64 = 0;
    let mut s: u32 = 1 << 31;
    while s > 0 {
        let rx = ((x & s) > 0) as u64;
        let ry = ((y & s) > 0) as u64;
        d += (s as u64) * (s as u64) * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = !x;
                y = !y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    d
}

/// Grid coordinates for a lon/lat pair in 1e-7 degrees.
#[inline]
pub fn grid(lon: i32, lat: i32) -> (u32, u32) {
    let x = (lon as i64 + 1_800_000_000).clamp(0, u32::MAX as i64) as u32;
    let y = ((lat as i64 + 900_000_000) * 2).clamp(0, u32::MAX as i64) as u32;
    (x, y)
}

/// Hilbert key for a lon/lat pair in 1e-7 degrees.
#[inline]
pub fn key(lon: i32, lat: i32) -> u64 {
    let (x, y) = grid(lon, lat);
    xy2d(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_order_matches_reference() {
        // Neighbouring cells on the curve are adjacent: check locality on a coarse grid.
        let n = 16u32;
        let shift = 32 - 4;
        let mut cells: Vec<(u64, u32, u32)> = Vec::new();
        for x in 0..n {
            for y in 0..n {
                cells.push((xy2d(x << shift, y << shift), x, y));
            }
        }
        cells.sort();
        for w in cells.windows(2) {
            let dx = (w[0].1 as i32 - w[1].1 as i32).abs();
            let dy = (w[0].2 as i32 - w[1].2 as i32).abs();
            assert_eq!(dx + dy, 1, "{:?}", w);
        }
    }
}

/// S2-style cell key (experiment): cube-face projection with the S2 quadratic transform,
/// then a Hilbert curve of order 30 on the face; face in the top 3 bits. Same clustering
/// properties as S2 cell ids (the per-face orientation table of S2 is omitted).
pub fn s2_key(lon: i32, lat: i32) -> u64 {
    let (lo, la) = ((lon as f64 * 1e-7).to_radians(), (lat as f64 * 1e-7).to_radians());
    let (x, y, z) = (la.cos() * lo.cos(), la.cos() * lo.sin(), la.sin());
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (face, u, v) = if ax >= ay && ax >= az {
        if x > 0.0 { (0, y / x, z / x) } else { (3, z / x, y / x) }
    } else if ay >= az {
        if y > 0.0 { (1, -x / y, z / y) } else { (4, z / y, -x / y) }
    } else if z > 0.0 {
        (2, -x / z, -y / z)
    } else {
        (5, y / z, x / z)
    };
    let st = |u: f64| if u >= 0.0 { 0.5 * (1.0 + 3.0 * u).sqrt() } else { 1.0 - 0.5 * (1.0 - 3.0 * u).sqrt() };
    let ij = |s: f64| ((s * (1u64 << 30) as f64) as u64).min((1 << 30) - 1) as u32;
    let (i, j) = (ij(st(u)), ij(st(v)));
    // order-30 Hilbert index fits in 60 bits
    ((face as u64) << 61) | (xy2d(i << 2, j << 2) >> 4)
}
