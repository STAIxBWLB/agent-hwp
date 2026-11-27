//! HWP equation scripts laid out as glyph runs and baseline-relative vector strokes.

use crate::display::{PageList, PathCmd};
use crate::fonts::FontStore;
use crate::shape::{ShapedRun, shape_plain, shape_text_with_font};
use hwp_model::{Document, Equation};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Literal(String),
    LBrace,
    RBrace,
    Delimiter(char),
    Sup,
    Sub,
    Hash,
    Amp,
    Space(f32),
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut Vec<Tok>| {
        if !word.is_empty() {
            out.push(Tok::Word(std::mem::take(word)));
        }
    };
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        let token = match c {
            '{' => Tok::LBrace,
            '}' => Tok::RBrace,
            '(' | ')' | '[' | ']' | '|' => Tok::Delimiter(c),
            '^' => Tok::Sup,
            '_' => Tok::Sub,
            '#' => Tok::Hash,
            '&' => Tok::Amp,
            '~' => Tok::Space(1.0),
            '`' => Tok::Space(0.25),
            '"' => Tok::Literal(chars.by_ref().take_while(|&c| c != '"').collect()),
            c if c.is_whitespace() => {
                flush(&mut word, &mut out);
                continue;
            }
            _ => {
                word.push(c);
                continue;
            }
        };
        flush(&mut word, &mut out);
        out.push(token);
    }
    flush(&mut word, &mut out);
    out
}

#[derive(Debug, Clone, Copy)]
enum MatrixKind {
    Bare,
    Parentheses,
    Brackets,
    Bars,
    Cases,
}

impl MatrixKind {
    fn delimiters(self) -> (Option<char>, Option<char>) {
        match self {
            Self::Bare => (None, None),
            Self::Parentheses => (Some('('), Some(')')),
            Self::Brackets => (Some('['), Some(']')),
            Self::Bars => (Some('|'), Some('|')),
            Self::Cases => (Some('{'), None),
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Row(Vec<Node>),
    Stack(Vec<Node>),
    Sym(String),
    Literal(String),
    Frac(Box<Node>, Box<Node>, bool),
    Script(Box<Node>, Option<Box<Node>>, Option<Box<Node>>),
    Sqrt(Box<Node>),
    Space(f32),
    Matrix(MatrixKind, Vec<Vec<Node>>),
    Delimited(Option<char>, Box<Node>, Option<char>),
    LargeOp(String, Option<Box<Node>>, Option<Box<Node>>),
}

struct Parser {
    toks: Vec<Tok>,
    i: usize,
    remaining_fraction_links: usize,
}

impl Parser {
    // Bound work and stack depth on deeply nested source scripts.
    const MAX_DEPTH: usize = 64;

    fn delimiter(&mut self) -> Option<char> {
        let delimiter = match self.toks.get(self.i)? {
            Tok::Delimiter(c) => Some(*c),
            Tok::LBrace => Some('{'),
            Tok::RBrace => Some('}'),
            Tok::Word(word) if word == "." => None,
            Tok::Word(word) if word.eq_ignore_ascii_case("lbrace") => Some('{'),
            Tok::Word(word) if word.eq_ignore_ascii_case("rbrace") => Some('}'),
            _ => return None,
        };
        self.i += 1;
        delimiter
    }

    fn eat(&mut self, token: Tok) -> bool {
        if self.toks.get(self.i) == Some(&token) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(self.toks.get(self.i), Some(Tok::Word(word)) if word.eq_ignore_ascii_case(keyword))
    }

    fn atom(&mut self, depth: usize) -> Option<Node> {
        if depth >= Self::MAX_DEPTH {
            return None;
        }
        let token = self.toks.get(self.i)?.clone();
        self.i += 1;
        match token {
            Tok::LBrace => {
                let inner = self.stack(depth + 1, false);
                self.eat(Tok::RBrace);
                Some(inner)
            }
            Tok::Space(width) => Some(Node::Space(width)),
            Tok::Delimiter(c) => Some(Node::Literal(c.to_string())),
            Tok::Literal(text) => Some(Node::Literal(text)),
            Tok::Word(word) => {
                let keyword = word.to_ascii_lowercase();
                match keyword.as_str() {
                    "sqrt" | "root" => Some(Node::Sqrt(Box::new(self.scripted(depth + 1)))),
                    "left" => {
                        let left = self.delimiter();
                        let inner = self.stack(depth + 1, true);
                        let right = if self.at_keyword("right") {
                            self.i += 1;
                            self.delimiter()
                        } else {
                            None
                        };
                        Some(Node::Delimited(left, Box::new(inner), right))
                    }
                    "matrix" | "pmatrix" | "bmatrix" | "dmatrix" | "cases"
                        if self.eat(Tok::LBrace) =>
                    {
                        let kind = match keyword.as_str() {
                            "pmatrix" => MatrixKind::Parentheses,
                            "bmatrix" => MatrixKind::Brackets,
                            "dmatrix" => MatrixKind::Bars,
                            "cases" => MatrixKind::Cases,
                            _ => MatrixKind::Bare,
                        };
                        Some(Node::Matrix(kind, self.matrix(depth + 1)))
                    }
                    "sum" | "int" | "oint" | "dint" | "tint" | "prod" | "union" | "inter"
                    | "lim" => Some(Node::LargeOp(keyword, None, None)),
                    _ => Some(Node::Sym(word)),
                }
            }
            _ => {
                self.i -= 1;
                None
            }
        }
    }

    fn matrix(&mut self, depth: usize) -> Vec<Vec<Node>> {
        let mut rows = Vec::new();
        loop {
            let mut row = vec![self.row(depth, true, false)];
            while self.eat(Tok::Amp) {
                row.push(self.row(depth, true, false));
            }
            rows.push(row);
            if !self.eat(Tok::Hash) {
                self.eat(Tok::RBrace);
                return rows;
            }
        }
    }

    fn scripted(&mut self, depth: usize) -> Node {
        let base = self.atom(depth).unwrap_or_else(|| Node::Row(Vec::new()));
        if depth >= Self::MAX_DEPTH {
            return base;
        }
        let operator = matches!(base, Node::LargeOp(..));
        let (mut upper, mut lower) = (None, None);
        loop {
            if self.eat(Tok::Sup) || operator && self.take_keyword("to") {
                upper = self.atom(depth + 1).map(Box::new);
            } else if self.eat(Tok::Sub) || operator && self.take_keyword("from") {
                lower = self.atom(depth + 1).map(Box::new);
            } else {
                break;
            }
        }
        if let Node::LargeOp(name, _, _) = base {
            Node::LargeOp(name, upper, lower)
        } else if upper.is_some() || lower.is_some() {
            Node::Script(Box::new(base), upper, lower)
        } else {
            base
        }
    }

    fn take_keyword(&mut self, keyword: &str) -> bool {
        if self.at_keyword(keyword) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn row(&mut self, depth: usize, matrix_cell: bool, stop_at_right: bool) -> Node {
        let mut items = Vec::new();
        let mut chained_fractions = 0;
        while let Some(token) = self.toks.get(self.i) {
            if matches!(token, Tok::RBrace | Tok::Hash)
                || matrix_cell && *token == Tok::Amp
                || stop_at_right && self.at_keyword("right")
            {
                break;
            }
            if self.eat(Tok::Amp) {
                continue;
            }
            // Hancom's infix fractions bind to the adjacent atoms.
            if self.at_keyword("over") || self.at_keyword("atop") {
                // Bound left-deep infix trees as well as explicit nesting.
                if depth >= Self::MAX_DEPTH {
                    break;
                }
                if chained_fractions != 0 {
                    // Extra links accumulate across nested groups, so share one budget.
                    let Some(remaining) = self.remaining_fraction_links.checked_sub(1) else {
                        break;
                    };
                    self.remaining_fraction_links = remaining;
                }
                chained_fractions += 1;
                let bar = self.at_keyword("over");
                self.i += 1;
                let numerator = items.pop().unwrap_or_else(|| Node::Row(Vec::new()));
                let denominator = self.scripted(depth);
                items.push(Node::Frac(Box::new(numerator), Box::new(denominator), bar));
                continue;
            }
            let before = self.i;
            let item = self.scripted(depth);
            if self.i == before {
                break;
            }
            items.push(item);
            chained_fractions = 0;
        }
        if items.len() == 1 {
            items.pop().expect("one item")
        } else {
            Node::Row(items)
        }
    }

    fn stack(&mut self, depth: usize, stop_at_right: bool) -> Node {
        let mut rows = vec![self.row(depth, false, stop_at_right)];
        while self.eat(Tok::Hash) {
            rows.push(self.row(depth, false, stop_at_right));
        }
        if rows.len() == 1 {
            rows.pop().expect("one row")
        } else {
            Node::Stack(rows)
        }
    }
}

fn parse(script: &str) -> Node {
    Parser {
        toks: tokenize(script),
        i: 0,
        remaining_fraction_links: Parser::MAX_DEPTH,
    }
    .stack(0, false)
}

/// 수식 토큰 → 표시 문자열(기호). 함수어/숫자/미지정은 원문.
fn sym_text(tok: &str) -> String {
    let m = match tok {
        "times" => "×",
        "div" => "÷",
        "cdot" | "dot" => "·",
        "pm" => "±",
        "mp" => "∓",
        "sum" | "SUM" => "∑",
        "int" | "INT" => "∫",
        "prod" | "PROD" => "∏",
        "oint" => "∮",
        "dint" => "∬",
        "tint" => "∭",
        "union" => "⋃",
        "inter" => "⋂",
        "inf" | "infinity" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "leq" | "<=" => "≤",
        "geq" | ">=" => "≥",
        "neq" | "!=" => "≠",
        "approx" => "≈",
        "to" | "rightarrow" | "->" => "→",
        "leftarrow" | "<-" => "←",
        "in" => "∈",
        "notin" => "∉",
        "cdots" => "⋯",
        "degree" => "°",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "tau" => "τ",
        "phi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "ALPHA" => "Α",
        "BETA" => "Β",
        "EPSILON" => "Ε",
        "ZETA" => "Ζ",
        "ETA" => "Η",
        "IOTA" => "Ι",
        "KAPPA" => "Κ",
        "MU" => "Μ",
        "NU" => "Ν",
        "RHO" => "Ρ",
        "TAU" => "Τ",
        "CHI" => "Χ",
        "GAMMA" => "Γ",
        "DELTA" => "Δ",
        "THETA" => "Θ",
        "LAMBDA" => "Λ",
        "PI" => "Π",
        "PROD_U" => "Π",
        "SIGMA" => "Σ",
        "PHI" => "Φ",
        "PSI" => "Ψ",
        "OMEGA" => "Ω",
        // 글꼴/그룹 명령은 버린다.
        "LEFT" | "RIGHT" | "rm" | "it" | "bold" | "ITALIC" | "roman" => "",
        other => return other.to_string(),
    };
    m.to_string()
}

/// Baseline-relative layout in points. Negative vertical offsets are above the baseline.
pub struct EqBox {
    pub width: f32,
    pub ascent: f32,
    pub descent: f32,
    runs: Vec<(ShapedRun, f32, f32)>,
    strokes: Vec<(Vec<PathCmd>, f32)>,
    color: u32,
}

impl EqBox {
    fn empty() -> Self {
        Self {
            width: 0.0,
            ascent: 0.0,
            descent: 0.0,
            runs: Vec::new(),
            strokes: Vec::new(),
            color: 0,
        }
    }

    fn place(&mut self, child: Self, x: f32, y: f32) {
        self.runs.extend(
            child
                .runs
                .into_iter()
                .map(|(run, dx, dy)| (run, dx + x, dy + y)),
        );
        for (mut path, width) in child.strokes {
            translate_path(&mut path, x, y);
            self.strokes.push((path, width));
        }
        self.width = self.width.max(x + child.width);
        self.ascent = self.ascent.max(child.ascent - y);
        self.descent = self.descent.max(child.descent + y);
    }

    fn rule(&mut self, x1: f32, x2: f32, y: f32, size: f32) {
        self.strokes.push((
            vec![PathCmd::MoveTo(x1, y), PathCmd::LineTo(x2, y)],
            (size * 0.05).max(0.6),
        ));
    }
}

fn translate_path(path: &mut [PathCmd], dx: f32, dy: f32) {
    for command in path {
        match command {
            PathCmd::MoveTo(x, y) | PathCmd::LineTo(x, y) => {
                *x += dx;
                *y += dy;
            }
            PathCmd::CubicTo(x1, y1, x2, y2, x3, y3) => {
                *x1 += dx;
                *x2 += dx;
                *x3 += dx;
                *y1 += dy;
                *y2 += dy;
                *y3 += dy;
            }
            PathCmd::Close => {}
        }
    }
}

struct Typesetter<'a> {
    store: &'a mut FontStore,
    doc: &'a Document,
    color: u32,
    font_name: Option<&'a str>,
}

impl Typesetter<'_> {
    fn symbol(&mut self, text: &str, size: f32, italic: bool) -> EqBox {
        if text.is_empty() {
            return EqBox::empty();
        }
        let named = self
            .font_name
            .and_then(|name| self.store.resolve_family_selection(name, false))
            .and_then(|selection| {
                shape_text_with_font(&selection.font, size, self.color, italic, text, false)
            });
        let run =
            named.or_else(|| shape_plain(self.store, self.doc, text, size, self.color, italic));
        let width = run
            .as_ref()
            .map_or(size * 0.5 * text.chars().count() as f32, |run| run.width_pt);
        EqBox {
            width,
            ascent: size * 0.72,
            descent: size * 0.22,
            runs: run.into_iter().map(|run| (run, 0.0, 0.0)).collect(),
            ..EqBox::empty()
        }
    }

    fn layout(&mut self, node: &Node, size: f32) -> EqBox {
        let mut result = EqBox::empty();
        match node {
            Node::Space(width) => result.width = size * width,
            Node::Sym(word) => {
                let text = sym_text(word);
                let italic =
                    text == *word && word.len() == 1 && word.as_bytes()[0].is_ascii_alphabetic();
                return self.symbol(&text, size, italic);
            }
            Node::Literal(text) => return self.symbol(text, size, false),
            Node::Row(items) => {
                for item in items {
                    result.place(self.layout(item, size), result.width, 0.0);
                }
            }
            Node::Stack(rows) => {
                let rows = rows
                    .iter()
                    .map(|row| vec![self.layout(row, size)])
                    .collect();
                return layout_grid(rows, size, 0.0);
            }
            Node::Matrix(kind, rows) => {
                let rows = rows
                    .iter()
                    .map(|row| row.iter().map(|cell| self.layout(cell, size)).collect())
                    .collect();
                let (left, right) = kind.delimiters();
                return enclose(layout_grid(rows, size, -size * 0.28), left, right, size);
            }
            Node::Delimited(left, inner, right) => {
                return enclose(self.layout(inner, size), *left, *right, size);
            }
            Node::Script(base, upper, lower) => {
                result = self.layout(base, size);
                let script_x = result.width;
                if let Some(upper) = upper {
                    result.place(self.layout(upper, size * 0.72), script_x, -size * 0.5);
                }
                if let Some(lower) = lower {
                    result.place(self.layout(lower, size * 0.72), script_x, size * 0.28);
                }
            }
            Node::LargeOp(op, upper, lower) => {
                let op = self.symbol(&sym_text(op), size * 1.16, false);
                let upper = upper.as_ref().map(|node| self.layout(node, size * 0.72));
                let lower = lower.as_ref().map(|node| self.layout(node, size * 0.72));
                let width = [Some(&op), upper.as_ref(), lower.as_ref()]
                    .into_iter()
                    .flatten()
                    .map(|b| b.width)
                    .fold(0.0, f32::max);
                if let Some(upper) = upper {
                    let y = -op.ascent - size * 0.12 - upper.descent;
                    let x = (width - upper.width) * 0.5;
                    result.place(upper, x, y);
                }
                if let Some(lower) = lower {
                    let y = op.descent + size * 0.12 + lower.ascent;
                    let x = (width - lower.width) * 0.5;
                    result.place(lower, x, y);
                }
                let x = (width - op.width) * 0.5;
                result.place(op, x, 0.0);
            }
            Node::Frac(num, den, bar) => {
                let num = self.layout(num, size * 0.95);
                let den = self.layout(den, size * 0.95);
                let width = num.width.max(den.width);
                let axis = -size * 0.28;
                let gap = size * 0.15;
                let num_position = ((width - num.width) * 0.5, axis - gap - num.descent);
                let den_position = ((width - den.width) * 0.5, axis + gap + den.ascent);
                result.place(num, num_position.0, num_position.1);
                result.place(den, den_position.0, den_position.1);
                if *bar {
                    result.rule(0.0, width, axis, size);
                }
            }
            Node::Sqrt(inner) => {
                let inner = self.layout(inner, size);
                let radical = self.symbol("√", size + inner.ascent * 0.3, false);
                let x = radical.width;
                result.place(radical, 0.0, 0.0);
                result.place(inner, x, 0.0);
                result.rule(x, result.width, -result.ascent, size);
            }
        }
        result
    }
}

fn layout_grid(rows: Vec<Vec<EqBox>>, size: f32, axis: f32) -> EqBox {
    let mut widths = vec![0.0_f32; rows.iter().map(Vec::len).max().unwrap_or(0)];
    let mut heights = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut ascent = 0.0_f32;
        let mut descent = 0.0_f32;
        for (cell, width) in row.iter().zip(&mut widths) {
            *width = width.max(cell.width);
            ascent = ascent.max(cell.ascent);
            descent = descent.max(cell.descent);
        }
        heights.push((ascent, descent));
    }
    let column_gap = 0.55 * size;
    let row_gap = 0.35 * size;
    let height = heights.iter().map(|(a, d)| a + d).sum::<f32>()
        + row_gap * rows.len().saturating_sub(1) as f32;
    let mut top = axis - height * 0.5;
    let mut result = EqBox::empty();
    for (row, (ascent, descent)) in rows.into_iter().zip(heights) {
        let mut left = 0.0;
        for (cell, width) in row.into_iter().zip(&widths) {
            let x = left + (width - cell.width) * 0.5;
            result.place(cell, x, top + ascent);
            left += width + column_gap;
        }
        top += ascent + descent + row_gap;
    }
    result.width = widths.iter().sum::<f32>() + column_gap * widths.len().saturating_sub(1) as f32;
    result
}

fn enclose(inner: EqBox, left: Option<char>, right: Option<char>, size: f32) -> EqBox {
    if left.is_none() && right.is_none() {
        return inner;
    }
    let top = -inner.ascent.max(size * 0.72) - size * 0.08;
    let bottom = inner.descent.max(size * 0.22) + size * 0.08;
    let mut result = EqBox::empty();
    let left_width = left.map_or(0.0, |ch| delimiter(&mut result, ch, 0.0, top, bottom, size));
    result.place(inner, left_width, 0.0);
    if let Some(ch) = right {
        let x = result.width;
        result.width += delimiter(&mut result, ch, x, top, bottom, size);
    }
    result.ascent = -top;
    result.descent = bottom;
    result
}

fn delimiter(result: &mut EqBox, ch: char, x: f32, top: f32, bottom: f32, size: f32) -> f32 {
    use PathCmd::{CubicTo, LineTo, MoveTo};
    let span = bottom - top;
    let mid = (top + bottom) * 0.5;
    let width = size * 0.28;
    let (outer, inner) = if matches!(ch, ')' | ']' | '}') {
        (x + width, x)
    } else {
        (x, x + width)
    };
    let path = match ch {
        '(' | ')' => vec![
            MoveTo(inner, top),
            CubicTo(
                outer,
                top + span * 0.22,
                outer,
                bottom - span * 0.22,
                inner,
                bottom,
            ),
        ],
        '[' | ']' => vec![
            MoveTo(inner, top),
            LineTo(outer, top),
            LineTo(outer, bottom),
            LineTo(inner, bottom),
        ],
        '{' | '}' => vec![
            MoveTo(inner, top),
            CubicTo(outer, top, inner, mid - span * 0.10, outer, mid),
            CubicTo(inner, mid + span * 0.10, outer, bottom, inner, bottom),
        ],
        _ => vec![
            MoveTo(x + width * 0.5, top),
            LineTo(x + width * 0.5, bottom),
        ],
    };
    result.strokes.push((path, (size * 0.05).max(0.6)));
    width + size * 0.12
}

pub fn typeset(store: &mut FontStore, doc: &Document, script: &str, size: f32) -> EqBox {
    Typesetter {
        store,
        doc,
        color: 0,
        font_name: None,
    }
    .layout(&parse(script), size.max(1.0))
}

pub(crate) fn typeset_equation(
    store: &mut FontStore,
    doc: &Document,
    equation: &Equation,
    height: f32,
) -> EqBox {
    let tree = parse(&equation.script);
    let mut typesetter = Typesetter {
        store,
        doc,
        color: equation.text_color,
        font_name: equation.font_name.as_deref(),
    };
    let size = if equation.base_unit == 0 {
        let probe = typesetter.layout(&tree, 12.0);
        (12.0 * height / (probe.ascent + probe.descent).max(1.0)).clamp(6.0, 18.0)
    } else {
        (equation.base_unit as f32 / 100.0).clamp(1.0, 144.0)
    };
    let mut result = typesetter.layout(&tree, size);
    result.color = equation.text_color;
    result
}

pub fn render_into(
    page: &mut PageList,
    eq: EqBox,
    ox: f32,
    baseline_y: f32,
    issues: &mut crate::issues::RenderIssueAccumulator,
) {
    use crate::display::Item;
    for (run, dx, dy) in eq.runs {
        crate::layout::push_run(page, ox + dx, baseline_y + dy, run, &[], issues);
    }
    for (mut commands, width) in eq.strokes {
        if !issues.charge_display_items(1) {
            return;
        }
        translate_path(&mut commands, ox, baseline_y);
        page.items.push(Item::Path {
            commands,
            fill: None,
            stroke: Some(crate::display::Stroke::solid(eq.color, width)),
            rule: crate::display::FillRule::NonZero,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 토크나이즈_기본() {
        let t = tokenize("a over b");
        assert_eq!(
            t,
            vec![
                Tok::Word("a".into()),
                Tok::Word("over".into()),
                Tok::Word("b".into())
            ]
        );
        let t2 = tokenize("x^2 _i");
        assert!(t2.contains(&Tok::Sup) && t2.contains(&Tok::Sub));
    }

    #[test]
    fn 파싱_분수() {
        match parse("a over b") {
            Node::Frac(_, _, true) => {}
            other => panic!("분수 아님: {other:?}"),
        }
        match parse("x atop y") {
            Node::Frac(_, _, false) => {}
            other => panic!("atop 아님: {other:?}"),
        }
    }

    #[test]
    fn 파싱_첨자() {
        match parse("x^2") {
            Node::Script(_, Some(_), None) => {}
            other => panic!("첨자 아님: {other:?}"),
        }
    }

    #[test]
    fn 파싱_근호_그룹() {
        match parse("sqrt {a+b}") {
            Node::Sqrt(_) => {}
            other => panic!("근호 아님: {other:?}"),
        }
    }

    #[test]
    fn matrix_variants_keep_cells_and_share_the_same_grid() {
        let document = Document::default();
        let mut store = FontStore::new_isolated();
        let bare = typeset(&mut store, &document, "MATRIX{1&22#333&4}", 10.0);
        assert!(bare.strokes.is_empty());
        assert!(bare.ascent + bare.descent > 18.0);
        for (script, strokes) in [
            ("PMATRIX{1&22#333&4}", 2),
            ("BMATRIX{1&22#333&4}", 2),
            ("DMATRIX{1&22#333&4}", 2),
            ("CASES{1&22#333&4}", 1),
        ] {
            let Node::Matrix(_, rows) = parse(script) else {
                panic!("{script}");
            };
            assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2]);
            assert!(matches!(&rows[0][1], Node::Sym(cell) if cell == "22"));
            let formatted = typeset(&mut store, &document, script, 10.0);
            assert_eq!(formatted.strokes.len(), strokes);
            assert!((formatted.width - bare.width - 4.0 * strokes as f32).abs() < 0.001);
            assert!(formatted.ascent > bare.ascent);
            assert!(formatted.descent > bare.descent);
        }
        let Node::Literal(text) = parse(r#""&""#) else {
            panic!("expected quoted text")
        };
        assert_eq!(text, "&");
    }

    #[test]
    fn large_operator_limits_extend_both_sides_of_the_baseline() {
        let document = Document::default();
        let mut store = FontStore::new_isolated();
        for (operator, script) in [
            ("sum", "sum_{i=1}^{n}"),
            ("int", "INT from {0} to {infinity}"),
        ] {
            assert!(matches!(parse(script), Node::LargeOp(_, Some(_), Some(_))));
            let operator = typeset(&mut store, &document, operator, 10.0);
            let limits = typeset(&mut store, &document, script, 10.0);
            assert!(limits.ascent > operator.ascent);
            assert!(limits.descent > operator.descent);
        }
    }

    #[test]
    fn paired_delimiters_enclose_a_nested_fraction() {
        let document = Document::default();
        let mut store = FontStore::new_isolated();
        let tall = typeset(
            &mut store,
            &document,
            "LEFT ( {a over b} over c RIGHT )",
            10.0,
        );
        let short = typeset(&mut store, &document, "LEFT[x RIGHT]", 10.0);
        assert_eq!(tall.strokes.len(), 4);
        assert_eq!(short.strokes.len(), 2);
        assert!(tall.ascent + tall.descent > short.ascent + short.descent);
        let delimiter = &tall.strokes[0].0;
        let [
            PathCmd::MoveTo(_, top),
            PathCmd::CubicTo(_, _, _, _, _, bottom),
        ] = delimiter.as_slice()
        else {
            panic!("expected a curved delimiter");
        };
        assert_eq!(*top, -tall.ascent);
        assert_eq!(*bottom, tall.descent);
    }

    #[test]
    fn nested_fraction_chains_and_extreme_font_sizes_are_bounded() {
        let mut script = "x".to_owned();
        for _ in 0..40 {
            script = format!("{{{script}{}}}", " over x".repeat(40));
        }
        let doc = Document::default();
        let mut fonts = FontStore::new_isolated();
        let expression = Equation {
            script,
            base_unit: u32::MAX,
            ..Equation::authored()
        };
        let result = typeset_equation(&mut fonts, &doc, &expression, 20.0);
        assert!(result.ascent.is_finite() && result.descent.is_finite());
        let size = |base_unit, fonts: &mut FontStore| {
            typeset_equation(
                fonts,
                &doc,
                &Equation {
                    script: "~".into(),
                    base_unit,
                    ..Equation::authored()
                },
                20.0,
            )
            .width
        };
        assert_eq!(size(u32::MAX, &mut fonts), size(14_400, &mut fonts));
    }

    #[test]
    fn equation_size_and_color_come_from_metadata() {
        let document = Document::default();
        let mut store = FontStore::new_isolated();
        let equation = Equation {
            script: "LEFT [ a over b RIGHT ]".into(),
            base_unit: 2_200,
            text_color: 0x0022_44a0,
            ..Default::default()
        };
        let sized = typeset_equation(&mut store, &document, &equation, 10.0);
        let same = typeset(&mut store, &document, &equation.script, 22.0);
        assert_eq!(sized.width, same.width);
        assert_eq!(sized.ascent, same.ascent);
        assert_eq!(sized.color, equation.text_color);
    }

    #[test]
    fn 기호_매핑() {
        assert_eq!(sym_text("alpha"), "α");
        assert_eq!(sym_text("sum"), "∑");
        assert_eq!(sym_text("x"), "x");
        assert_eq!(sym_text("rm"), "");
    }
}
