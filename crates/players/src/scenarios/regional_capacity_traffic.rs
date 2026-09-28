//! Distributed showcase: four residents per 18-block cell, at most six resident/incoming
//! claimants, distinct slots and asynchronous adjacent-region travel. No common rendezvous.
//! Failed navigation resolves claims against measured region membership; capacity is never
//! recovered with a timer while a player may still be walking toward the old target.
use super::*;
use pawa_schema::regions::{Bounds, CAPACITY, SCENARIO, SLOTS};

async fn visit(ctx: &Ctx, x: f64, z: f64, timeout: u64, bounds: Bounds) -> bool {
    if let Some(p) = ctx.position() {
        // Resolve the large camera turn before locomotion begins; the move controller then only
        // makes small route-following corrections instead of turning and walking simultaneously.
        ctx.look_until_facing((x, p.1 + 1.62, z), 10.0, 5_000).await;
    }
    ctx.move_with_bounds(x, z, timeout, 1.5, Some(bounds)).await
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let anchor = ctx.anchor();
    let mut round = 0usize;
    while !ctx.target_met() {
        let random = (ctx.rng_next() * 1_000_000.0) as usize;
        let (dest, target, bounds, before) = {
            let mut traffic = ctx.engine().shared.regional_traffic.lock();
            let home = traffic.homes[index];
            let neighbors = traffic.neighbors(home.region);
            // Start with a local traverse so staggered player starts do not launch one wave.
            // After that most moves transfer, while occasional local visits preserve variation.
            let mut regions = neighbors;
            if !regions.is_empty() {
                let len = regions.len();
                regions.rotate_left(random % len);
            }
            if round == 0 || random.is_multiple_of(4) {
                regions.insert(0, home.region);
            }
            regions.push(home.region);
            let dest = regions
                .into_iter()
                .find_map(|r| traffic.reserve(index, r, random % SLOTS.len()));
            let target = dest.map(|d| traffic.point(d));
            let bounds =
                traffic.bounds(home.region, dest.map_or(home.region, |d| d.region), anchor);
            let before = json!({"revision": traffic.revision, "claims": traffic.counts(), "home": home,
                "destination": dest, "capacity": CAPACITY, "player": index, "round": round});
            (dest, target, bounds, before)
        };
        ctx.record(
            "observe",
            &format!("region-reserve-{index}-{round}"),
            "Reserve a distinct regional destination before navigating.",
            before,
        );
        let Some(target) = target else {
            // Full destinations do not induce busy polling or force entry; keep the existing
            // residence and look around before retrying when other transfers free a place.
            glance_at_nearest_peer(ctx, None).await;
            sleep_ms(700 + (random % 700) as u64).await;
            round += 1;
            continue;
        };
        let target = (anchor.0 + target.0, anchor.1 + target.1);
        let arrived = visit(ctx, target.0, target.1, 7_000, bounds).await;
        let position = ctx.position();
        let complete = {
            let mut traffic = ctx.engine().shared.regional_traffic.lock();
            let region = position.and_then(|p| traffic.region_at(p.0 - anchor.0, p.2 - anchor.1));
            traffic.finish(index, region, arrived)
        };
        let cancelled = {
            let mut traffic = ctx.engine().shared.regional_traffic.lock();
            let region = ctx
                .position()
                .and_then(|p| traffic.region_at(p.0 - anchor.0, p.2 - anchor.1));
            let had_pending = traffic.pending[index].is_some();
            traffic.cancel_at_home(index, region);
            had_pending && traffic.pending[index].is_none()
        };
        let after = {
            let traffic = ctx.engine().shared.regional_traffic.lock();
            json!({"revision": traffic.revision, "claims": traffic.counts(), "capacity": CAPACITY,
                "player": index, "round": round, "destination": dest, "arrived": complete,
                "cancelled_at_source": cancelled, "reached_address": arrived, "position": ctx.position(), "scenario": SCENARIO})
        };
        ctx.record(
            "observe",
            &format!("region-finish-{index}-{round}"),
            "Record actual arrival or a failed trip without discarding live reservations.",
            after,
        );
        // Deliberate look happens while stationary; continuous target tracking would couple
        // camera rotation to travel and obscure the regional motion in a first-person view.
        if !ctx.target_met() {
            glance_at_nearest_peer(ctx, None).await;
        }
        round += 1;
    }
}
