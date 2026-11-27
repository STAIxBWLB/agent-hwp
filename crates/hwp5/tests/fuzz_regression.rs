#[path = "../../../fuzz/support/hwp5.rs"]
mod harness;
#[path = "../../../fuzz/support/replay.rs"]
mod replay;

#[test]
fn replays_synthetic_reader_corpus() {
    replay::replay("fuzz_hwp5_read", harness::exercise);
}
