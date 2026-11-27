#[path = "../../../fuzz/support/hwpx.rs"]
mod harness;
#[path = "../../../fuzz/support/replay.rs"]
mod replay;

#[test]
fn replays_synthetic_reader_corpus() {
    replay::replay("fuzz_hwpx_read", harness::exercise);
}
