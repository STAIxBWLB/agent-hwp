[한국어](authoring.ko.md) · [English](authoring.md)

# Authoring documents

How to produce new HWP/HWPX files: from a DocumentSpec or TemplateSpec, as a Korean official document (공문서), or regenerated from the JSON IR. For people and agents writing documents rather than editing one.

## Compose a document from a spec

`compose` compiles a v1 JSON/YAML specification into paragraphs, runs, styles, lists, tables,
equations, fields, headers/footers, page numbers and sections. v2 wraps that document and adds images
with accessibility descriptions, a closed SVG→PNG fallback, and HWPX native rectangular text boxes.
The v2 fallback policy is stated per target; omitting it fails as native-only.

```sh
# Validate schema, references, table spans, assets and native support without writing
ahwp compose examples/document-spec-v1/basic.json -o /tmp/basic.hwpx --dry-run --report

# Publish the validated file atomically
ahwp compose examples/document-spec-v1/comprehensive.yaml -o /tmp/report.hwpx --report
```

`template` allows only a typed `value`/`if`/`each` AST, with no string interpolation and no expression
evaluation. `reference_hwpx` surgically fills only the named text and fields of an existing package;
regenerating structure requires an explicit strict gate.

Normative schemas are in [`schemas/`](../../schemas/) and the contracts in
[13-document-spec-v1](../design/13-document-spec-v1.md),
[14-template-spec-v1](../design/14-template-spec-v1.md) and
[15-document-spec-v2](../design/15-document-spec-v2.md). Examples are in [`examples/`](../../examples/).

## Write an official document (공문서)

Korean official documents follow a fixed notation and layout. That layer is built into the binary
rather than left to a prompt.

**Profiles** (`--preset`) set the margins, numbering and paragraph shapes of a document type:
`official` (기안문·공문서), `report` (보고서), `plan` (사업계획서), `notice` (공고문),
`minutes` (회의록) and `press` (보도자료). Korean aliases (`기안문`, `보고서`, `공고`, …) normalize
onto the same six. Nested lists render the statutory eight-level item marks.

**Templates** (`--template`) go further: each brings its own profile *and* native 두문/결문 tables,
pre-filled with `{{slots}}`.

| Slug | Korean alias | Notes |
|---|---|---|
| `gian-internal` | 기안문-내부결재 | Internal approval |
| `gian-external` | 기안문-대외시행 | External dispatch |
| `gongmun-basic` | 공문서-기본 | Multi-recipient form (`{{수신자}}`) |
| `report` | 보고서 | |
| `plan` | 사업계획서 | |
| `minutes` | 회의록 | |
| `notice` | 공고문 | |
| `press` | 보도자료 | |

**Frames** fill those header and footer blocks directly: `--doc-head 기관명=…`,
`--doc-foot 발신명의=…`, `--notice-head`, `--notice-foot` and `--press-head`. Since v0.10.0 both
`--preset` and the frame flags **override** a template's own defaults instead of being refused.

**Filling** A generated document is itself a template. `ahwp slots` lists what is still unfilled and
`ahwp fill` replaces `{{name}}`, preserving the rest of the hwpx package byte for byte. Both commands
read a slot the same way: the name is any text without braces or control characters
(`{{성 명}}`, `{{사업명(국문)}}`), padding inside the braces (`{{ name }}`) is trimmed, and so are
requested names. Values are inserted literally in one pass, so a value that spells a slot is not
filled again. An unmatched slot is an error and nothing is written unless `--allow-partial` is
given; with it, even a fill that matched nothing publishes the input unchanged and reports zero
counts.

**Form fields** Korean public forms rarely carry slots. `ahwp slots --forms` also lists label cells
(성명, 연락처, ...), inline `라벨: 값` text and slots, each with a key, label, source, confidence
and occurrence count. `ahwp fill --forms` fills them from the same `--data` or `--set` values in one
pass: the cell next to or below a label cell, the text after an inline label, `학년(  )반` blanks,
`□동의` checkboxes and `(비고:   )` blanks. A non-empty value cell, and the inline text after
`라벨:` (up to a comma, semicolon, the next `라벨:` or the line end; a bracket the label sits in,
as in `(한문:   )`, and a bracketed note such as `(서명/인)` stay), is overwritten, as kordoc does;
the report warns for each overwrite. A cell holding a field (누름틀), picture or table is never
written, and values are inserted literally. Keys match with spaces, colons and parentheses
ignored, and `--json` adds the `unmatched` keys. The rules are ported from [kordoc](https://github.com/chrisryugj/kordoc) (MIT).
This path goes through the IR and writes `.hwpx` only: the sections it changed are rewritten the
way `ahwp edit` writes them, and every other section and package entry is copied byte for byte.

**Linting** `ahwp lint` applies ten rules to `.md`, `.hwp` and `.hwpx` files (or to stdin markdown
with `-`): seven notation rules (date `2026. 8. 20.`, time, money, the `붙임:` colon and its
numbering, the closing `끝.`, punctuation), one against decorative item marks (`■ ▶ ▲ ◆ ● ※`, which
belong as `□ ○` or as nested lists), and two structural rules for statutory item marks and
Roman-numeral headings. Only the two structural rules are error severity; every notation finding is
a warning. The command is advisory and exits 0 unless `--strict` is given.

**Table styling** `ahwp edit --style-tables <preset>` applies the profile's table look. Single-column
tables are skipped, and applying it twice produces byte-identical output.

## Report blocks: table of contents and section strips

Both are opt-in and leave the output unchanged when absent. They apply to markdown input to
`ahwp new` and to DocumentSpec v2 (`report_blocks`).

```bash
ahwp new --from report.md -o report.hwpx --preset report --toc --section-strips --font-dir ./fonts
```

- `--section-strips` turns each `### N. Title` heading into a 1x3 strip table (shaded number,
  spacer, bottom-bordered title). `cat --format markdown` reads it back as the same heading.
- `--toc` inserts a front table of contents over the `##` and `###` headings (styles `개요 2` and
  `개요 3`; the `#` title is not listed). The body page number restarts at 1 after it. Page
  numbers come from a layout pass on the written file, repeated until they stop changing.
  Pagination depends on fonts: give the document's fonts with `--font-dir` or `HWP_FONT_DIR`,
  otherwise a warning reports the substitutions and Hangul may break pages differently.
- In DocumentSpec v2: `"report_blocks": {"toc": true, "section_strips": true}`. Headings are
  paragraphs whose style is named `개요 2` or `개요 3`.
- Limits: the TOC must fit in the first page's body area, above the bottom and footer margins
  (an inline table never splits); a longer one is refused
  with a next step. Only the first section's headings are listed. A DocumentSpec `page_number.start` other
  than 1 conflicts with the TOC's restart at 1, so leave it unset with `toc`. For `.hwp` output the
  writer synthesizes line caches from `HWP_FONT_DIR` only; `--font-dir` reaches the TOC layout
  pass but not that synthesis, so set `HWP_FONT_DIR` for `.hwp`. `cat --format markdown` reads
  any 1x3 table of that shape under a `개요 3` paragraph back as `### N. Title`, whether or not it
  came from `--section-strips`.
- Pagination against Hancom: the TOC page numbers come from ahwp's own layout pass, and Hancom
  can paginate differently. On the measured 16-entry sample (not a general bound), `.hwp` matches
  Hancom on all 16 entries since the table-anchor line rule
  ([#644](https://github.com/STAIxBWLB/agent-hwp/issues/644)): the written line caches equal a
  Hancom re-save's in 66 of 67 body paragraphs, the last off by 1 HU. `.hwpx` matched on 5 of
  16 (measured before #644), the others off by 1-2 pages, because the `.hwpx` carries no line
  layout cache, so Hancom re-wraps the lines and fits more characters per line than ahwp
  ([#643](https://github.com/STAIxBWLB/agent-hwp/issues/643)). Check the TOC in Hancom before
  distributing the document.

## Wrap Hangul text

Markdown output uses the HWPX `KEEP_WORD` token (HWP5 paragraph-shape bit 7
set), which allows wrapping within a Hangul word. For a style with
`BREAK_WORD` (bit 7 clear), ahwp moves a space-delimited Hangul word to the
next line when it does not fit. This applies to newly laid-out body text
whose words contain only Hangul syllables. Tokens with punctuation, digits
or other scripts use glyph wrapping, as do cells, captions and text boxes.
A word longer than an empty line still wraps within the word. Existing
usable line caches retain their stored breaks.

## Regenerate a document from the JSON IR

A document can be recreated from scratch rather than edited in place.

```sh
ahwp convert report.hwpx -o report.json   # hwp/hwpx → JSON IR (--embed-bin also embeds images)
ahwp new --from report.json -o regen.hwpx # JSON IR → new document
```

Regeneration is verified (`crates/hwp-cli/tests/regen.rs`) by validation passing, identical `ahwp cat`
output, an identical table map (count, rows/columns, merges, cell widths) and byte-identical secPr and
tabProperties slices. What still differs is by design: the line layout cache (recomputed by Hancom
Office on open), the preview image and settings.xml.

## Native HWPX charts

A DocumentSpec body can include a `chart` block with dimensions in millimeters and DrawingML data.
Supported kinds include `column`, `bar`, `line`, `pie`, `doughnut`, `area`, and `scatter`.
Pie and doughnut use one series with nonnegative values and a positive total. Scatter uses
numeric `x_values`; omitted X coordinates are filled with consecutive positions starting at 1.
The HWP5 writer reports `chart_control_unrepresentable` for these charts.

```json
{
  "type": "chart",
  "width_mm": 120,
  "height_mm": 65,
  "data": {
    "kind": "column",
    "title": "Enrollment",
    "categories": ["2025", "2026"],
    "series": [{"name": "Students", "values": [120, 145], "color": "#173F73"}]
  }
}
```
