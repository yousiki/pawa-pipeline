use super::*;

pub(super) async fn run(ctx: &mut Ctx) {
    build_tower::run(ctx, true).await;
}
