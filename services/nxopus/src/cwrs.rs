// Ported from libopus 1.6.1, celt/cwrs.c. Copyright 2007-2009 Timothy B.
// Terriberry, 2007-2009 Xiph.Org Foundation, Jean-Marc Valin.
// BSD-3-Clause, see LICENSE in this crate.

//! PVQ codewords (RFC 6716 §4.3.4.2): the index of a vector of `k` signed
//! pulses in `n` dimensions, decoded back to the vector.

use crate::range::RangeDecoder;

const ROWS: usize = 15;
const COLS: usize = 177;

/// U(N,K) for min(N,K) < 15 and max(N,K) <= 176 (the widest split band),
/// built by its recurrence (U(0,0) = 1, U(N,0) = U(0,K) = 0 otherwise); values past 32 bits wrap and are never read.
static U: [[u32; COLS]; ROWS] = build();

const fn build() -> [[u32; COLS]; ROWS] {
    let mut big = [[0u32; COLS]; ROWS];
    big[0][0] = 1;
    let mut n = 1;
    while n < ROWS {
        let mut k = 1;
        while k < COLS {
            big[n][k] = big[n - 1][k]
                .wrapping_add(big[n][k - 1])
                .wrapping_add(big[n - 1][k - 1]);
            k += 1;
        }
        n += 1;
    }
    big
}

fn u(n: usize, k: usize) -> u32 {
    U[n.min(k)][n.max(k)]
}

/// V(N,K): the number of codewords of `k` pulses in `n` dimensions.
fn v(n: usize, k: usize) -> u32 {
    u(n, k).wrapping_add(u(n, k + 1))
}

/// Decodes `k` pulses in `n` dimensions into `y`; returns the sum of the
/// squares.
pub fn decode_pulses(y: &mut [i32], n: usize, k: usize, dec: &mut RangeDecoder) -> f32 {
    let i = dec.uint(v(n, k));
    cwrsi(n, k, i, y)
}

fn cwrsi(mut n: usize, mut k: usize, mut i: u32, y: &mut [i32]) -> f32 {
    let mut yy = 0f32;
    let mut at = 0;
    let mut put = |val: i32, at: &mut usize| {
        y[*at] = val;
        *at += 1;
        yy += (val * val) as f32;
    };
    while n > 2 {
        if k >= n {
            let row = &U[n];
            let mut p = row[k + 1];
            let s = -((i >= p) as i32);
            i -= p & s as u32;
            let k0 = k;
            let q = row[n];
            if q > i {
                k = n;
                loop {
                    k -= 1;
                    p = U[k][n];
                    if p <= i {
                        break;
                    }
                }
            } else {
                p = row[k];
                while p > i {
                    k -= 1;
                    p = row[k];
                }
            }
            i -= p;
            put((k0 as i32 - k as i32 + s) ^ s, &mut at);
        } else {
            let mut p = U[k][n];
            let q = U[k + 1][n];
            if p <= i && i < q {
                i -= p;
                put(0, &mut at);
            } else {
                let s = -((i >= q) as i32);
                i -= q & s as u32;
                let k0 = k;
                loop {
                    k -= 1;
                    p = U[k][n];
                    if p <= i {
                        break;
                    }
                }
                i -= p;
                put((k0 as i32 - k as i32 + s) ^ s, &mut at);
            }
        }
        n -= 1;
    }
    let p = 2 * k as u32 + 1;
    let s = -((i >= p) as i32);
    i -= p & s as u32;
    let k0 = k;
    k = ((i + 1) >> 1) as usize;
    if k != 0 {
        i -= 2 * k as u32 - 1;
    }
    put((k0 as i32 - k as i32 + s) ^ s, &mut at);
    let s = -(i as i32);
    put((k as i32 + s) ^ s, &mut at);
    yy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_counts() {
        // V(N,K) from RFC 6716 §4.3.4.2's definition: V(2,1)=4, V(3,2)=18.
        assert_eq!(v(2, 1), 4);
        assert_eq!(v(3, 2), 18);
        assert_eq!(v(176, 1), 352);
    }
}
