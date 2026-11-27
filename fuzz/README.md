# Reader fuzzing

This package is separate from the stable workspace (the root `Cargo.toml` has
`exclude = ["fuzz"]`), so `cargo test --workspace` never builds a fuzz binary. The stable
integration tests compile the same harness modules through `#[path]` and replay the checked-in
`seed-*` inputs, plus any reviewed `regression-*` input, so what a campaign finds is replayed
afterwards without libFuzzer.

| Target | Entry points |
|---|---|
| `fuzz_hwp5_read` | HWP5 reader with and without a password, strict and tolerant record scans, HWP 3.00 reader |
| `fuzz_hwpx_read` | HWPX reader with and without a password, section and header XML, HWPML reader |
| `fuzz_wmf_dib` | WMF renderer and DIBStretchBlt bitmap decoder |
| `fuzz_equation_markdown` | Equation typesetting, Markdown import, HTML import |

## Campaign

[cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz/tutorial.html) needs nightly Rust. Run the
campaign on the remote host, not on a workstation:

```sh
dot remote run "$PWD" -- bash fuzz/campaign.sh 60
```

`fuzz/campaign.sh` installs `cargo-fuzz` when it is missing, builds one target at a time, and prints
one `target=<name> seconds=<n> exit=<code> execs=<n> new_artifacts=<n>` line per target. The same
loop runs by hand, with the limits #623 AC1 sets (no panic, no OOM over 256 MiB per input, no
timeout over 2 s per input):

```sh
export CARGO_BUILD_JOBS=2
for target in fuzz_hwp5_read fuzz_hwpx_read fuzz_wmf_dib fuzz_equation_markdown; do
    cargo +nightly fuzz run "$target" -- -max_total_time=60 -timeout=2 -rss_limit_mb=256 -malloc_limit_mb=256
done
```

The RSS limit is what enforces AC1's per-input ceiling: `-malloc_limit_mb` caps a single
allocation, so an input holding several smaller buffers would pass that limit while exceeding the
bound. If a target's own baseline ever trips the 256 MiB limit, report the measured baseline and run
that target at baseline + 256 with the number stated.

## The committed inputs are synthetic

No genuine or third-party document is committed here; the data policy in `AGENTS.md` applies. Each
committed input carries its own evidence that it was built rather than saved by an application:

| Input | Evidence |
|---|---|
| `fuzz_hwp5_read/seed-synthetic.hwp` | A document written by this repository's own HWP5 writer: the CFB `Application` property is `hwp-cli` (`crates/hwp5/src/write.rs`). |
| `fuzz_hwp5_read/seed-legacy.hwp` | The 30-byte HWP 3.00 signature (`crates/hwp5/src/hwp3.rs` `SIGNATURE`) inside a 128-byte header, its compression flag set (`info[124]`, absolute byte 154), and a payload that starts with a gzip header but is not a valid stream. The flag matters: at `info[124]=0` the reader takes the uncompressed branch and never reaches the decompressor (`hwp3.rs`), which is why the byte is 1 here. The reader therefore enters its gzip/deflate step and rejects the truncated stream; a saved file is never in that shape. |
| `fuzz_hwp5_read/seed-truncated-cfb` | The 8-byte CFB signature and nothing else. |
| `fuzz_hwp5_read/seed-short-record` | A 5-byte truncated record header. |
| `fuzz_hwpx_read/seed-zip` | A hand-built ZIP: the `mimetype` entry stored uncompressed and first, then two deflated XML entries. |
| `fuzz_hwpx_read/seed-header.xml`, `seed-section.xml`, `seed-legacy.hml` | Minimal XML and HWPML documents written as text. |
| `fuzz_wmf_dib/seed-wmf-header` | An 18-byte WMF header and an EOF record, 24 bytes total. |
| `fuzz_wmf_dib/seed-bitmap-info` | A 1x1x24 `BITMAPINFOHEADER` and one pixel. |
| `fuzz_equation_markdown/seed-equation`, `seed-text.md`, `seed-table.html` | Three short text documents. |

Campaign mutations and raw crash artifacts stay local: `fuzz/.gitignore` keeps the corpus to
`seed-*` and the artifacts to `regression-*`. When a campaign finds a crash, review the minimized
input for genuine content, commit it as `fuzz/artifacts/<target>/regression-*`, and the stable replay
suite covers it from then on.
