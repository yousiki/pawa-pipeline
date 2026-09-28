//! Geometry and capacity ledger shared by capture staging and the regional traffic script.
//! Claims include BOTH the departure region and the reserved destination until a measured
//! boundary crossing completes a transfer. A timeout never silently frees a traveller's place.

pub const SCENARIO: &str = "regional_capacity_traffic";
pub const INITIAL_PER_REGION: usize = 4;
pub const CAPACITY: usize = 6;
pub const PITCH: f64 = 18.0;
pub const SLOTS: [(f64, f64); 9] = [
    (-5.0, -5.0),
    (5.0, -5.0),
    (-5.0, 5.0),
    (5.0, 5.0),
    (0.0, -5.0),
    (-5.0, 0.0),
    (5.0, 0.0),
    (0.0, 5.0),
    (0.0, 0.0),
];

#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub min_x: f64,
    pub max_x: f64,
    pub min_z: f64,
    pub max_z: f64,
}

impl Bounds {
    pub fn contains(&self, x: f64, z: f64) -> bool {
        x >= self.min_x && x <= self.max_x && z >= self.min_z && z <= self.max_z
    }

    /// Pick an inward recovery heading with a four-block clear geometric margin. Physical
    /// recovery stops after 2.5 blocks and still samples its bounds; this is not a terrain test.
    pub fn recovery_target(&self, x: f64, z: f64, phase: f64) -> Option<(f64, f64)> {
        if !self.contains(x, z) {
            return None;
        }
        (0..8).find_map(|i| {
            let angle = phase + i as f64 * std::f64::consts::FRAC_PI_4;
            let target = (x + 4.0 * angle.cos(), z + 4.0 * angle.sin());
            (self.contains(target.0 - 1.0, target.1 - 1.0)
                && self.contains(target.0 + 1.0, target.1 + 1.0))
            .then_some(target)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Address {
    pub region: usize,
    pub slot: usize,
}

#[derive(Debug)]
pub struct Traffic {
    pub homes: Vec<Address>,
    pub pending: Vec<Option<Address>>,
    pub side: usize,
    pub revision: u64,
}

impl Traffic {
    pub fn new(players: usize) -> Self {
        let regions = players.div_ceil(INITIAL_PER_REGION).max(1);
        let side = ((regions as f64).sqrt().ceil() as usize).max(2);
        Self {
            homes: (0..players)
                .map(|i| Address {
                    region: i / INITIAL_PER_REGION,
                    slot: i % INITIAL_PER_REGION,
                })
                .collect(),
            pending: vec![None; players],
            side,
            revision: 0,
        }
    }

    pub fn center(&self, region: usize) -> (f64, f64) {
        let mid = (self.side - 1) as f64 / 2.0;
        (
            ((region % self.side) as f64 - mid) * PITCH,
            ((region / self.side) as f64 - mid) * PITCH,
        )
    }

    pub fn point(&self, address: Address) -> (f64, f64) {
        let c = self.center(address.region);
        let s = SLOTS[address.slot];
        (c.0 + s.0, c.1 + s.1)
    }

    /// Adjacent cells form a convex rectangle. Keep path nodes one block inside its outer edge
    /// so controller rounding and player width do not send bodies into an unreserved third cell.
    pub fn bounds(&self, from: usize, to: usize, anchor: (f64, f64)) -> Bounds {
        let a = self.center(from);
        let b = self.center(to);
        let half = PITCH / 2.0 - 1.0;
        Bounds {
            min_x: anchor.0 + a.0.min(b.0) - half,
            max_x: anchor.0 + a.0.max(b.0) + half,
            min_z: anchor.1 + a.1.min(b.1) - half,
            max_z: anchor.1 + a.1.max(b.1) + half,
        }
    }

    pub fn region_at(&self, x: f64, z: f64) -> Option<usize> {
        let mid = (self.side - 1) as f64 / 2.0;
        let col = (x / PITCH + mid + 0.5).floor() as isize;
        let row = (z / PITCH + mid + 0.5).floor() as isize;
        if col >= 0 && row >= 0 && col < self.side as isize && row < self.side as isize {
            Some(row as usize * self.side + col as usize)
        } else {
            None
        }
    }

    pub fn counts(&self) -> Vec<usize> {
        let mut counts = vec![0; self.side * self.side];
        for (home, pending) in self.homes.iter().zip(&self.pending) {
            counts[home.region] += 1;
            if let Some(p) = pending.filter(|p| p.region != home.region) {
                counts[p.region] += 1;
            }
        }
        counts
    }

    pub fn neighbors(&self, region: usize) -> Vec<usize> {
        (0..self.side * self.side)
            .filter(|&other| {
                (region % self.side).abs_diff(other % self.side)
                    + (region / self.side).abs_diff(other / self.side)
                    == 1
            })
            .collect()
    }

    /// Call only under the episode's shared mutex. No lock is held during navigation.
    pub fn reserve(&mut self, player: usize, region: usize, offset: usize) -> Option<Address> {
        if self.pending[player].is_some() {
            return self.pending[player];
        }
        if region >= self.side * self.side {
            return None;
        }
        let home = self.homes[player];
        if region != home.region
            && (!self.neighbors(home.region).contains(&region) || self.counts()[region] >= CAPACITY)
        {
            return None;
        }
        let address = (0..SLOTS.len())
            .map(|s| Address {
                region,
                slot: (s + offset) % SLOTS.len(),
            })
            .find(|a| !self.homes.contains(a) && !self.pending.contains(&Some(*a)))?;
        self.pending[player] = Some(address);
        self.revision += 1;
        Some(address)
    }

    /// Navigation return codes are not evidence of membership: use the measured world position.
    pub fn finish(&mut self, player: usize, measured_region: Option<usize>, arrived: bool) -> bool {
        let Some(dest) = self.pending[player] else {
            return false;
        };
        if measured_region == Some(dest.region)
            && (arrived || dest.region != self.homes[player].region)
        {
            self.homes[player] = dest;
            self.pending[player] = None;
            self.revision += 1;
            return true;
        }
        false
    }

    /// Navigation has stopped. If the body remains in its held source REGION, cancellation is
    /// safe; requiring a return to one exact slot caused failed transfers to hold two cells forever.
    pub fn cancel_at_home(&mut self, player: usize, measured_region: Option<usize>) {
        if measured_region == Some(self.homes[player].region)
            && self.pending[player].take().is_some()
        {
            self.revision += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_from_edges_points_inward_without_crossing_unclaimed_cells() {
        let t = Traffic::new(64);
        let b = t.bounds(0, 1, (0.0, 0.0));
        for x in [b.min_x, b.min_x + 0.2, b.max_x - 0.2, b.max_x] {
            for z in [b.min_z, b.min_z + 0.2, b.max_z - 0.2, b.max_z] {
                for phase in 0..32 {
                    let (tx, tz) = b.recovery_target(x, z, phase as f64 * 0.2).unwrap();
                    assert!(b.contains(tx - 1.0, tz - 1.0));
                    assert!(b.contains(tx + 1.0, tz + 1.0));
                    for step in 0..=20 {
                        let f = step as f64 / 20.0;
                        assert!(b.contains(x + f * (tx - x), z + f * (tz - z)));
                    }
                }
            }
        }
        assert!(b.recovery_target(b.min_x - 0.1, b.min_z, 0.0).is_none());
    }
    #[test]
    fn bounded_adjacent_routes_cannot_enter_a_third_region() {
        let t = Traffic::new(128);
        for from in 0..t.side * t.side {
            for to in t.neighbors(from) {
                let bounds = t.bounds(from, to, (0.0, 0.0));
                for x in (bounds.min_x.floor() as i32)..=(bounds.max_x.ceil() as i32) {
                    for z in (bounds.min_z.floor() as i32)..=(bounds.max_z.ceil() as i32) {
                        if bounds.contains(x as f64 + 0.5, z as f64 + 0.5) {
                            assert!(
                                matches!(t.region_at(x as f64 + 0.5, z as f64 + 0.5), Some(r) if r == from || r == to)
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn crossing_releases_source_even_when_exact_address_is_unreachable() {
        let mut t = Traffic::new(64);
        t.reserve(0, 1, 0).unwrap();
        t.cancel_at_home(0, Some(1));
        assert!(t.pending[0].is_some());
        assert!(t.finish(0, Some(1), false));
        assert_eq!(t.counts()[0], 3);
        assert_eq!(t.counts()[1], 5);
        assert!(t.pending[0].is_none());
    }
    #[test]
    fn staging_and_geometry_scale_without_overlapping_addresses() {
        for n in [1, 4, 64, 128, 257] {
            let t = Traffic::new(n);
            assert!(t.counts().iter().all(|&c| c <= INITIAL_PER_REGION));
            for a in &t.homes {
                let p = t.point(*a);
                assert_eq!(t.region_at(p.0, p.1), Some(a.region));
            }
            assert_eq!(t.region_at(1e6, -1e6), None);
        }
    }
    #[test]
    fn full_region_rejects_claims_and_timeout_keeps_both_places() {
        let mut t = Traffic::new(64);
        assert!(t.reserve(0, 1, 0).is_some());
        assert!(t.reserve(1, 1, 1).is_some());
        assert_eq!(t.counts()[1], CAPACITY);
        assert!(t.reserve(2, 1, 0).is_none());
        assert!(!t.finish(0, Some(0), false));
        t.cancel_at_home(0, None);
        assert_eq!(t.counts()[1], CAPACITY);
        t.cancel_at_home(0, Some(0));
        assert_eq!(t.counts()[1], CAPACITY - 1);
        assert!(t.reserve(2, 1, 0).is_some());
    }
    #[test]
    fn arrivals_and_rejections_preserve_capacity_under_interleaved_requests() {
        let mut t = Traffic::new(128);
        for step in 0..20000 {
            let p = step % 128;
            if let Some(dest) = t.pending[p] {
                assert!(t.finish(p, Some(dest.region), true));
            } else {
                let neighbors = t.neighbors(t.homes[p].region);
                t.reserve(
                    p,
                    neighbors[(step / 128) % neighbors.len()],
                    step % CAPACITY,
                );
            }
            assert!(t.counts().iter().all(|&c| c <= CAPACITY));
            let mut addresses = t.homes.clone();
            addresses.extend(t.pending.iter().flatten());
            addresses.sort_by_key(|a| (a.region, a.slot));
            assert!(addresses.windows(2).all(|a| a[0] != a[1]));
        }
    }
}
