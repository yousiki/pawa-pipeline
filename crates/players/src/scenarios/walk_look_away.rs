use super::*;

pub(super) async fn run(ctx: &mut Ctx) {
    walk_look::run(ctx, true).await;
}
