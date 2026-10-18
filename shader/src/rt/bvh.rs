//! Building the wide BVH: a binned surface-area-heuristic binary tree,
//! then collapsed four to a node by opening the child with the largest
//! surface area until a node has four.

use alloc::vec::Vec;

use super::{EMPTY, LEAF, LEAF_MAX, MAX_DEPTH, NODE_WORDS, TRI_WORDS};

#[derive(Clone, Copy, Debug)]
pub struct Triangle {
    pub v: [[f32; 3]; 3],
    /// What a hit reports (a material, a mesh and triangle number, ...).
    pub id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildError {
    Empty,
    /// Over 2^28 triangles.
    TooMany,
    /// A tree deeper than the traversal stack allows (`MAX_DEPTH`).
    TooDeep,
    /// A vertex that is not a finite number.
    NotFinite,
}

pub struct Bvh {
    /// `NODE_WORDS` per node.
    pub nodes: Vec<u32>,
    /// `TRI_WORDS` per triangle, in leaf order.
    pub tris: Vec<u32>,
    pub node_count: u32,
    pub tri_count: u32,
    pub depth: u32,
    pub bounds: ([f32; 3], [f32; 3]),
}

#[derive(Clone, Copy)]
struct Aabb {
    min: [f32; 3],
    max: [f32; 3],
}

impl Aabb {
    const EMPTY: Aabb = Aabb {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };

    fn grow(&mut self, o: &Aabb) {
        for a in 0..3 {
            self.min[a] = self.min[a].min(o.min[a]);
            self.max[a] = self.max[a].max(o.max[a]);
        }
    }

    fn area(&self) -> f32 {
        let d = [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ];
        if d[0] < 0.0 || d[1] < 0.0 || d[2] < 0.0 {
            return 0.0;
        }
        2.0 * (d[0] * d[1] + d[1] * d[2] + d[2] * d[0])
    }
}

#[derive(Clone, Copy)]
struct Prim {
    b: Aabb,
    c: [f32; 3],
}

enum BNode {
    Leaf { first: u32, count: u32 },
    Inner { l: u32, r: u32 },
}

struct Builder<'a> {
    prims: Vec<Prim>,
    /// Primitive numbers, partitioned as the tree is built.
    idx: Vec<u32>,
    nodes: Vec<(Aabb, BNode)>,
    tris: &'a [Triangle],
}

const BINS: usize = 16;
/// Binary depth past which a node splits at the median whatever the cost.
const BINARY_DEPTH: u32 = 30;

impl Builder<'_> {
    fn bounds(&self, lo: usize, hi: usize) -> (Aabb, Aabb) {
        let (mut b, mut c) = (Aabb::EMPTY, Aabb::EMPTY);
        for &i in &self.idx[lo..hi] {
            let p = &self.prims[i as usize];
            b.grow(&p.b);
            c.grow(&Aabb { min: p.c, max: p.c });
        }
        (b, c)
    }

    fn build(&mut self, lo: usize, hi: usize, depth: u32) -> u32 {
        let n = (hi - lo) as u32;
        let (b, cb) = self.bounds(lo, hi);
        let leaf = |s: &mut Self, b: Aabb| {
            s.nodes.push((
                b,
                BNode::Leaf {
                    first: lo as u32,
                    count: n,
                },
            ));
            (s.nodes.len() - 1) as u32
        };
        if n == 1 {
            return leaf(self, b);
        }
        // Axis of the widest spread of centroids.
        let ext = [
            cb.max[0] - cb.min[0],
            cb.max[1] - cb.min[1],
            cb.max[2] - cb.min[2],
        ];
        let axis = if ext[0] >= ext[1] && ext[0] >= ext[2] {
            0
        } else if ext[1] >= ext[2] {
            1
        } else {
            2
        };
        let mut mid = lo + n as usize / 2;
        let mut split_sah = false;
        if ext[axis] > 0.0 && depth < BINARY_DEPTH {
            // Binned SAH.
            let mut bins = [(Aabb::EMPTY, 0u32); BINS];
            let scale = BINS as f32 / ext[axis];
            let bin_of = |c: f32| (((c - cb.min[axis]) * scale) as usize).min(BINS - 1);
            for &i in &self.idx[lo..hi] {
                let p = &self.prims[i as usize];
                let k = bin_of(p.c[axis]);
                bins[k].0.grow(&p.b);
                bins[k].1 += 1;
            }
            // Cost of splitting after bin k, from sweeps in both directions.
            let mut right = [0.0f32; BINS];
            let mut acc = Aabb::EMPTY;
            let mut cnt = 0u32;
            for k in (1..BINS).rev() {
                acc.grow(&bins[k].0);
                cnt += bins[k].1;
                right[k] = acc.area() * cnt as f32;
            }
            let mut left = Aabb::EMPTY;
            let mut lcnt = 0u32;
            let (mut best, mut best_k) = (f32::INFINITY, 0usize);
            for k in 0..BINS - 1 {
                left.grow(&bins[k].0);
                lcnt += bins[k].1;
                let cost = left.area() * lcnt as f32 + right[k + 1];
                if lcnt > 0 && lcnt < n && cost < best {
                    best = cost;
                    best_k = k;
                }
            }
            if best.is_finite() {
                let total = b.area().max(1e-30);
                // Traversal costs about as much as intersecting a triangle.
                let split_cost = 0.5 + best / total;
                if n <= LEAF_MAX && split_cost >= n as f32 {
                    return leaf(self, b);
                }
                // Partition by bin.
                let mut i = lo;
                let mut j = hi;
                while i < j {
                    let p = &self.prims[self.idx[i] as usize];
                    if bin_of(p.c[axis]) <= best_k {
                        i += 1;
                    } else {
                        j -= 1;
                        self.idx.swap(i, j);
                    }
                }
                if i > lo && i < hi {
                    mid = i;
                    split_sah = true;
                }
            }
        }
        if !split_sah {
            if n <= LEAF_MAX && ext[axis] <= 0.0 {
                // Same centroid: nothing separates them.
                return leaf(self, b);
            }
            // Median of the centroids on the axis.
            let prims = &self.prims;
            self.idx[lo..hi].sort_unstable_by(|&a, &b| {
                prims[a as usize].c[axis]
                    .partial_cmp(&prims[b as usize].c[axis])
                    .unwrap_or(core::cmp::Ordering::Equal)
            });
        }
        let at = self.nodes.len();
        self.nodes.push((b, BNode::Leaf { first: 0, count: 0 }));
        let l = self.build(lo, mid, depth + 1);
        let r = self.build(mid, hi, depth + 1);
        self.nodes[at].1 = BNode::Inner { l, r };
        at as u32
    }
}

/// Builds the wide tree over `tris`.
pub fn build(tris: &[Triangle]) -> Result<Bvh, BuildError> {
    if tris.is_empty() {
        return Err(BuildError::Empty);
    }
    if tris.len() >= 1 << 28 {
        return Err(BuildError::TooMany);
    }
    let mut prims = Vec::with_capacity(tris.len());
    for t in tris {
        let mut b = Aabb::EMPTY;
        for v in &t.v {
            if !v.iter().all(|x| x.is_finite()) {
                return Err(BuildError::NotFinite);
            }
            b.grow(&Aabb { min: *v, max: *v });
        }
        let c = [
            (b.min[0] + b.max[0]) * 0.5,
            (b.min[1] + b.max[1]) * 0.5,
            (b.min[2] + b.max[2]) * 0.5,
        ];
        prims.push(Prim { b, c });
    }
    let mut bld = Builder {
        prims,
        idx: (0..tris.len() as u32).collect(),
        nodes: Vec::new(),
        tris,
    };
    let root = bld.build(0, tris.len(), 0);
    let mut out = Out {
        b: &bld,
        words: Vec::new(),
        depth: 0,
    };
    // The root is always an inner node of the wide tree.
    out.node(root, 1);
    if out.depth > MAX_DEPTH {
        return Err(BuildError::TooDeep);
    }
    let depth = out.depth;
    let words = out.words;
    let mut tri_words = Vec::with_capacity(tris.len() * TRI_WORDS);
    for &i in &bld.idx {
        let t = &bld.tris[i as usize];
        let (v0, v1, v2) = (t.v[0], t.v[1], t.v[2]);
        tri_words.extend(v0.iter().map(|x| x.to_bits()));
        tri_words.extend((0..3).map(|a| (v1[a] - v0[a]).to_bits()));
        tri_words.extend((0..3).map(|a| (v2[a] - v0[a]).to_bits()));
        tri_words.extend([t.id, 0, 0]);
    }
    let rb = &bld.nodes[root as usize].0;
    Ok(Bvh {
        node_count: (words.len() / NODE_WORDS) as u32,
        nodes: words,
        tris: tri_words,
        tri_count: tris.len() as u32,
        depth,
        bounds: (rb.min, rb.max),
    })
}

struct Out<'a> {
    b: &'a Builder<'a>,
    words: Vec<u32>,
    depth: u32,
}

impl Out<'_> {
    /// The wide node for binary node `at` (inner, or a leaf alone at the
    /// root); its index.
    fn node(&mut self, at: u32, depth: u32) -> u32 {
        self.depth = self.depth.max(depth);
        let mut kids: Vec<u32> = alloc::vec![at];
        if let BNode::Inner { l, r } = self.b.nodes[at as usize].1 {
            kids = alloc::vec![l, r];
        }
        // Open the inner child with the largest area until there are four.
        while kids.len() < 4 {
            let pick = kids
                .iter()
                .enumerate()
                .filter(|(_, &k)| matches!(self.b.nodes[k as usize].1, BNode::Inner { .. }))
                .max_by(|a, b| {
                    let (x, y) = (
                        self.b.nodes[*a.1 as usize].0.area(),
                        self.b.nodes[*b.1 as usize].0.area(),
                    );
                    x.partial_cmp(&y).unwrap_or(core::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i);
            let Some(i) = pick else { break };
            let BNode::Inner { l, r } = self.b.nodes[kids[i] as usize].1 else {
                break;
            };
            kids[i] = l;
            kids.insert(i + 1, r);
        }
        let id = (self.words.len() / NODE_WORDS) as u32;
        let base = self.words.len();
        self.words.resize(base + NODE_WORDS, 0);
        for c in 0..4 {
            self.words[base + c] = EMPTY;
            for a in 0..3 {
                self.words[base + 4 + 4 * a + c] = f32::INFINITY.to_bits();
                self.words[base + 16 + 4 * a + c] = f32::NEG_INFINITY.to_bits();
            }
        }
        for (c, &k) in kids.iter().enumerate() {
            let (bx, node) = &self.b.nodes[k as usize];
            let r = match node {
                BNode::Leaf { first, count } => LEAF | first << 3 | (count - 1),
                BNode::Inner { .. } => self.node(k, depth + 1),
            };
            self.words[base + c] = r;
            for a in 0..3 {
                self.words[base + 4 + 4 * a + c] = bx.min[a].to_bits();
                self.words[base + 16 + 4 * a + c] = bx.max[a].to_bits();
            }
        }
        id
    }
}
