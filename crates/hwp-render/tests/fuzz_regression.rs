#[path = "../../../fuzz/support/equation_markdown.rs"]
mod equation_markdown;
#[path = "../../../fuzz/support/replay.rs"]
mod replay;
#[path = "../../../fuzz/support/wmf_dib.rs"]
mod wmf_dib;

#[test]
fn replays_synthetic_wmf_and_dib_corpus() {
    replay::replay("fuzz_wmf_dib", wmf_dib::exercise);
}

#[test]
fn replays_synthetic_equation_and_markup_corpus() {
    replay::replay("fuzz_equation_markdown", equation_markdown::exercise);
}
