// The turn-driving contract must be reachable entirely from `gait_core::embed`.
// If any of these paths break, the single-public-door invariant regressed.
#[allow(unused_imports)]
use gait_core::embed::{
    resolve_profile, run_loop, user_turn_events, CliSink, Diag, LoopConfig, ResolvedProfile,
    StreamItem, TurnDriver, TurnResult, TurnSink, TurnSource, TurnStart,
};

#[test]
fn embed_is_the_single_public_door() {
    // Compile-only: the imports above must resolve through `embed`.
}
