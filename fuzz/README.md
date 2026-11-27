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

`fuzz/campaign.sh` installs `cargo-fuzz` when it is missing, builds one target at a time, and runs
two passes per target, printing one `target=<name> pass=<ac1|asan> seconds=<n> exit=<code>
execs=<n> new_artifacts=<n>` line each.

**The AC1 pass** is the one #623's criteria are read from: no sanitizer, so RSS is the heap plus a
small runtime baseline, and the limits are the criterion's own (no panic, no OOM over 256 MiB per
input, no timeout over 2 s per input).

```sh
export CARGO_BUILD_JOBS=2
for target in fuzz_hwp5_read fuzz_hwpx_read fuzz_wmf_dib fuzz_equation_markdown; do
    cargo +nightly fuzz run "$target" --sanitizer none -- \
        -max_total_time=60 -timeout=2 -rss_limit_mb=256 -malloc_limit_mb=256
done
```

`-malloc_limit_mb` caps a single allocation, so it cannot bound what one input accumulates; the RSS
limit is what enforces the 256 MiB ceiling. Running this pass without a sanitizer is what keeps that
number meaningful: ASAN's quarantine and shadow grow the process far past the heap, so the same cap
under ASAN reports memory that no input caused.

**The memory-safety pass** keeps ASAN, with `-rss_limit_mb=2048` stated for what it is — that pass's
process cap, not AC1:

```sh
for target in fuzz_hwp5_read fuzz_hwpx_read fuzz_wmf_dib fuzz_equation_markdown; do
    cargo +nightly fuzz run "$target" -- \
        -max_total_time=60 -timeout=2 -rss_limit_mb=2048 -malloc_limit_mb=256
done
```

A target that trips the AC1 pass has its artifact replayed alone before it counts as a finding:

```sh
cargo +nightly fuzz run <target> --sanitizer none -- \
    -timeout=2 -rss_limit_mb=256 -malloc_limit_mb=256 <artifact>
```

The replay carries the same three bounds as the campaign's AC1 pass, so a repeated input is judged
by the criterion rather than by a looser command: dropping `-timeout=2` or `-malloc_limit_mb=256`
would let a genuine per-input timeout or oversized allocation replay clean. A replay that reproduces
is a per-input bound to harden. One that does not is a campaign-length artifact, reported as such
rather than fixed by inventing a limit.

## The committed inputs are synthetic

No genuine or third-party document is committed here; the data policy in `AGENTS.md` applies. Each
committed input carries its own evidence that it was built rather than saved by an application:

| Input | Evidence |
|---|---|
| `fuzz_hwp5_read/seed-synthetic.hwp` | A document written by this repository's own HWP5 writer: the CFB `Application` property is `hwp-cli` (`crates/hwp5/src/write.rs`). |
| `fuzz_hwp5_read/seed-legacy.hwp` | The 30-byte HWP 3.00 signature (`crates/hwp5/src/hwp3.rs` `SIGNATURE`) inside a 128-byte header, its compression flag set (`info[124]`, absolute byte 154), and a 295-byte payload that is not a valid deflate stream. The flag matters: at `info[124]=0` the reader takes the uncompressed branch and never reaches the decompressor (`hwp3.rs`), which is why the byte is 1 here. The payload begins with zeros and holds no gzip magic - `1f 8b 08` sits at absolute offset 128, inside the header block, never in the payload - so the reader enters the compressed step, takes its raw-deflate branch for want of a gzip header, and rejects the stream. A saved file is never in that shape. |
| `fuzz_hwp5_read/seed-truncated-cfb` | The 8-byte CFB signature and nothing else. |
| `fuzz_hwp5_read/seed-short-record` | A 5-byte truncated record header. |
| `fuzz_hwpx_read/seed-zip` | A hand-built ZIP: the `mimetype` entry stored uncompressed and first, then two deflated XML entries. |
| `fuzz_hwpx_read/seed-header.xml`, `seed-section.xml`, `seed-legacy.hml` | Minimal XML and HWPML documents written as text. |
| `fuzz_wmf_dib/seed-wmf-header` | An 18-byte WMF header and an EOF record, 24 bytes total. |
| `fuzz_wmf_dib/seed-bitmap-info` | A 1x1x24 `BITMAPINFOHEADER` and one pixel. |
| `*/seed-fuzz-1`, `*/seed-fuzz-2` | Inputs the campaign generated itself: libFuzzer mutations of the synthetic seeds above, never of a saved document. Kept because the campaign reached them and the replay suite should keep reaching them. |
| `fuzz_equation_markdown/seed-equation`, `seed-text.md`, `seed-table.html` | Three short text documents. |

Campaign mutations and raw crash artifacts stay local: `fuzz/.gitignore` keeps the corpus to
`seed-*` and the artifacts to `regression-*`. When a campaign finds a crash, review the minimized
input for genuine content, commit it as `fuzz/artifacts/<target>/regression-*`, and the stable replay
suite covers it from then on.
