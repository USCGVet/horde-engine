//! Uniform 2D spatial grid, rebuilt every tick with a counting sort.
//!
//! Entities are bucketed by (group, cell), where a group is whatever a game wants to query separately:
//! teams, factions, vehicles versus infantry. A query for one group never touches the others, so a crowd
//! searching for a handful of enemies skips past its own members for free (docs/DESIGN.md, D7). The grid
//! keeps copies of positions in bucket order, so queries read contiguous memory, and a parallel pass can
//! move entities while querying this tick-start snapshot.

pub struct Grid {
    cell: f32,
    cols: usize,
    groups: usize,
    /// Bucket `b = group * cells + cell`; `start[b]..start[b + 1]` are its slots.
    start: Vec<u32>,
    cursor: Vec<u32>,
    bucket_of: Vec<u32>,
    entity: Vec<u32>,
    x: Vec<f32>,
    z: Vec<f32>,
}

const SKIPPED: u32 = u32::MAX;

impl Grid {
    /// A grid over the square `0..world_size` on both axes, with `cell`-sized cells and `groups` groups.
    pub fn new(world_size: f32, cell: f32, groups: usize) -> Grid {
        assert!(groups > 0, "a grid needs at least one group");
        let cols = ((world_size / cell).ceil() as usize).max(1);
        Grid {
            cell,
            cols,
            groups,
            start: Vec::new(),
            cursor: Vec::new(),
            bucket_of: Vec::new(),
            entity: Vec::new(),
            x: Vec::new(),
            z: Vec::new(),
        }
    }

    fn cells(&self) -> usize {
        self.cols * self.cols
    }

    /// Re-sorts entities `0..pos_x.len()` into the grid. `group_of(i)` gives entity `i`'s group, or
    /// `None` to leave it out (dead, inactive). Positions outside the world are clamped to its edge cells.
    pub fn rebuild(&mut self, pos_x: &[f32], pos_z: &[f32], mut group_of: impl FnMut(usize) -> Option<u8>) {
        let buckets = self.groups * self.cells();
        self.start.clear();
        self.start.resize(buckets + 1, 0);
        self.bucket_of.clear();
        for i in 0..pos_x.len() {
            match group_of(i) {
                Some(group) => {
                    debug_assert!((group as usize) < self.groups);
                    let b = group as usize * self.cells() + self.cell_at(pos_x[i], pos_z[i]);
                    self.bucket_of.push(b as u32);
                    self.start[b + 1] += 1;
                }
                None => self.bucket_of.push(SKIPPED),
            }
        }
        for b in 0..buckets {
            self.start[b + 1] += self.start[b];
        }
        let total = self.start[buckets] as usize;
        self.entity.resize(total, 0);
        self.x.resize(total, 0.0);
        self.z.resize(total, 0.0);
        self.cursor.clear();
        self.cursor.extend_from_slice(&self.start[..buckets]);
        for (i, &b) in self.bucket_of.iter().enumerate() {
            if b != SKIPPED {
                let slot = self.cursor[b as usize] as usize;
                self.cursor[b as usize] += 1;
                self.entity[slot] = i as u32;
                self.x[slot] = pos_x[i];
                self.z[slot] = pos_z[i];
            }
        }
    }

    /// The entity in a slot returned by a query.
    #[inline]
    pub fn entity(&self, slot: usize) -> u32 {
        self.entity[slot]
    }

    /// Tick-start x position of the entity in `slot`.
    #[inline]
    pub fn x(&self, slot: usize) -> f32 {
        self.x[slot]
    }

    /// Tick-start z position of the entity in `slot`.
    #[inline]
    pub fn z(&self, slot: usize) -> f32 {
        self.z[slot]
    }

    /// The closest entity of `group` within `radius` of (x, z), as (slot, squared distance).
    ///
    /// Searches outward one ring of cells at a time and stops once no unsearched cell can hold anything
    /// closer than the best hit so far. Ties go to the later slot, which is deterministic.
    pub fn nearest_in_group(&self, x: f32, z: f32, group: u8, radius: f32) -> Option<(usize, f32)> {
        let base = group as usize * self.cells();
        let (cx, cz) = self.coords(x, z);
        let rings = (radius / self.cell).ceil() as isize;
        let mut best: Option<(usize, f32)> = None;
        let mut best_d2 = radius * radius;
        for ring in 0..=rings {
            if best.is_some() {
                // Everything in this ring is at least (ring - 1) cells away.
                let reach = (ring - 1).max(0) as f32 * self.cell;
                if reach * reach > best_d2 {
                    break;
                }
            }
            let mut visit = |gx: isize, gz: isize| {
                if gx < 0 || gz < 0 || gx >= self.cols as isize || gz >= self.cols as isize {
                    return;
                }
                let b = base + gz as usize * self.cols + gx as usize;
                for slot in self.start[b] as usize..self.start[b + 1] as usize {
                    let d2 = (self.x[slot] - x).powi(2) + (self.z[slot] - z).powi(2);
                    if d2 <= best_d2 {
                        best_d2 = d2;
                        best = Some((slot, d2));
                    }
                }
            };
            if ring == 0 {
                visit(cx, cz);
                continue;
            }
            for gx in cx - ring..=cx + ring {
                visit(gx, cz - ring);
                visit(gx, cz + ring);
            }
            for gz in cz - ring + 1..=cz + ring - 1 {
                visit(cx - ring, gz);
                visit(cx + ring, gz);
            }
        }
        best
    }

    /// Calls `f(slot, dx, dz, d2)` for entities of `group` within `radius` of (x, z), where (dx, dz) points
    /// from the entity to (x, z), until `f` returns false. Meant for short ranges such as crowd separation:
    /// keep `radius` near the cell size so only a few cells are visited. Visiting order is fixed, so an
    /// early stop is deterministic.
    pub fn for_each_near(&self, x: f32, z: f32, group: u8, radius: f32, mut f: impl FnMut(usize, f32, f32, f32) -> bool) {
        let base = group as usize * self.cells();
        let (x0, z0) = self.coords(x - radius, z - radius);
        let (x1, z1) = self.coords(x + radius, z + radius);
        let r2 = radius * radius;
        for gz in z0..=z1 {
            for gx in x0..=x1 {
                let b = base + gz as usize * self.cols + gx as usize;
                for slot in self.start[b] as usize..self.start[b + 1] as usize {
                    let (dx, dz) = (x - self.x[slot], z - self.z[slot]);
                    let d2 = dx * dx + dz * dz;
                    if d2 <= r2 && !f(slot, dx, dz, d2) {
                        return;
                    }
                }
            }
        }
    }

    /// Counts entities of each group within `radius` of (x, z) into `counts`, which must have one entry
    /// per group.
    pub fn count_near(&self, x: f32, z: f32, radius: f32, counts: &mut [u32]) {
        assert_eq!(counts.len(), self.groups, "one count per group");
        counts.fill(0);
        let (x0, z0) = self.coords(x - radius, z - radius);
        let (x1, z1) = self.coords(x + radius, z + radius);
        let r2 = radius * radius;
        for (group, count) in counts.iter_mut().enumerate() {
            let base = group * self.cells();
            for gz in z0..=z1 {
                for gx in x0..=x1 {
                    let b = base + gz as usize * self.cols + gx as usize;
                    for slot in self.start[b] as usize..self.start[b + 1] as usize {
                        if (self.x[slot] - x).powi(2) + (self.z[slot] - z).powi(2) <= r2 {
                            *count += 1;
                        }
                    }
                }
            }
        }
    }

    fn coords(&self, x: f32, z: f32) -> (isize, isize) {
        let max = self.cols as isize - 1;
        let to_cell = |v: f32| ((v / self.cell) as isize).clamp(0, max);
        (to_cell(x), to_cell(z))
    }

    fn cell_at(&self, x: f32, z: f32) -> usize {
        let (cx, cz) = self.coords(x, z);
        cz as usize * self.cols + cx as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horde_core::rng;

    fn scatter(n: usize, size: f32, seed: u64) -> (Vec<f32>, Vec<f32>, Vec<u8>) {
        let x = (0..n).map(|i| rng::unit(seed, 0, i, 1) * size).collect();
        let z = (0..n).map(|i| rng::unit(seed, 0, i, 2) * size).collect();
        let g = (0..n).map(|i| u8::from(rng::unit(seed, 0, i, 3) < 0.3)).collect();
        (x, z, g)
    }

    #[test]
    fn nearest_matches_brute_force() {
        let (px, pz, group) = scatter(3000, 500.0, 7);
        let mut grid = Grid::new(500.0, 32.0, 2);
        grid.rebuild(&px, &pz, |i| (i % 10 != 0).then_some(group[i]));
        for q in 0..500 {
            let (qx, qz) = (rng::unit(9, 0, q, 1) * 500.0, rng::unit(9, 0, q, 2) * 500.0);
            for g in 0..2u8 {
                let expected = (0..px.len())
                    .filter(|&i| i % 10 != 0 && group[i] == g)
                    .map(|i| (px[i] - qx).powi(2) + (pz[i] - qz).powi(2))
                    .filter(|&d2| d2 <= 70.0 * 70.0)
                    .fold(None, |best: Option<f32>, d2| Some(best.map_or(d2, |b| b.min(d2))));
                let got = grid.nearest_in_group(qx, qz, g, 70.0).map(|(slot, d2)| {
                    assert_eq!(group[grid.entity(slot) as usize], g);
                    d2
                });
                assert_eq!(got, expected, "query {q} group {g}");
            }
        }
    }

    #[test]
    fn for_each_near_finds_exactly_the_group_within_radius() {
        let (px, pz, group) = scatter(4000, 200.0, 11);
        let mut grid = Grid::new(200.0, 4.0, 2);
        grid.rebuild(&px, &pz, |i| Some(group[i]));
        for q in 0..200 {
            let (qx, qz) = (rng::unit(5, 0, q, 1) * 200.0, rng::unit(5, 0, q, 2) * 200.0);
            let mut got: Vec<u32> = Vec::new();
            grid.for_each_near(qx, qz, 1, 6.0, |slot, _, _, _| {
                got.push(grid.entity(slot));
                true
            });
            got.sort_unstable();
            let expected: Vec<u32> = (0..px.len())
                .filter(|&i| group[i] == 1 && (px[i] - qx).powi(2) + (pz[i] - qz).powi(2) <= 36.0)
                .map(|i| i as u32)
                .collect();
            assert_eq!(got, expected, "query {q}");
        }
    }

    #[test]
    fn counts_per_group() {
        let (px, pz, group) = scatter(2000, 300.0, 3);
        let mut grid = Grid::new(300.0, 25.0, 2);
        grid.rebuild(&px, &pz, |i| Some(group[i]));
        let mut counts = [0u32; 2];
        grid.count_near(150.0, 150.0, 40.0, &mut counts);
        for g in 0..2u8 {
            let expected = (0..px.len())
                .filter(|&i| group[i] == g && (px[i] - 150.0).powi(2) + (pz[i] - 150.0).powi(2) <= 1600.0)
                .count() as u32;
            assert_eq!(counts[g as usize], expected);
        }
    }
}
