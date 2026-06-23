//! Block-aware Markdown renderer for the chat transcript.
//!
//! Replaces `tui-markdown`, which flattens everything into a borderless `Text`
//! and therefore cannot preserve code indentation, draw boxes, or align tables.
//! Here we parse with `pulldown-cmark` and walk the event stream while keeping a
//! small inline-style stack and a notion of the *current block*. Each block is
//! laid out to a fixed column width and emitted as fully-formed **visual lines**
//! (`Vec<Span<'static>>`) — already wrapped, boxed, padded, and aligned — so the
//! caller only prepends a bullet/indent and never re-wraps. That contract is
//! what keeps the chat view's follow-scroll math exact: emitted line count ==
//! on-screen line count.
//!
//! Code blocks are the priority feature: every fenced/indented block is drawn as
//! a full box with a language label and syntax highlighting (via `syntect` using
//! the pure-Rust fancy-regex engine, no Oniguruma), preserving whitespace and
//! hard-splitting over-long lines rather than word-wrapping. GFM tables are
//! collected in full, column widths fitted to the available width, and rendered
//! with box-drawing borders honouring per-column alignment. Prose, headings,
//! lists, block quotes, and thematic breaks word-wrap with inline styles intact.
//!
//! All colours are tuned for a dark background (the app default); `syntect`'s
//! own background is dropped so highlighted code blends with the terminal.

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::view::theme::Palette;

// --- syntect singletons (loaded once, on first markdown render with code) ----

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

/// Bundled syntax definitions (newline-terminated variants so `LinesWithEndings`
/// feeds `highlight_line` correctly). Loaded once and shared for the process.
fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Bundled dark theme for code highlighting. `base16-ocean.dark` reads well on
/// the app's default dark background; only the foreground is used.
fn theme() -> &'static Theme {
    THEME.get_or_init(|| ThemeSet::load_defaults().themes["base16-ocean.dark"].clone())
}

/// Map a `syntect` style to a ratatui [`Style`], keeping only the foreground RGB
/// so the terminal background shows through (code boxes are not filled).
fn syntect_fg(s: syntect::highlighting::Style) -> Style {
    Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b))
}

// --- public API --------------------------------------------------------------

/// Render `md` into final visual lines laid out to exactly `width` columns.
///
/// Each inner `Vec<Span>` is one on-screen line (already wrapped/boxed/aligned).
/// The caller prepends a bullet/indent and must NOT re-wrap. All spans own their
/// text (`'static`). Top-level blocks are separated by a single blank line.
pub fn render(md: &str, palette: &Palette, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(md, opts);

    let mut r = Renderer::new(palette, width);
    for ev in parser {
        r.event(ev);
    }
    r.finish()
}

// --- inline style stack ------------------------------------------------------

/// One pushed inline context. `Link` carries the destination so we can append
/// ` (url)` when the link closes. (Inline `code` arrives as a standalone
/// `Event::Code`, not a Start/End pair, so it isn't represented here.)
#[derive(Clone)]
enum Inline {
    Bold,
    Italic,
    Strike,
    Link(String),
}

// --- table buffering ---------------------------------------------------------

/// A table accumulated until its `End` so column widths can be fitted before any
/// row is drawn. Cells are styled inline spans; `head` is rendered bold.
struct TableBuf {
    aligns: Vec<Alignment>,
    head: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    in_head: bool,
    cur_row: Vec<Vec<Span<'static>>>,
}

// --- the renderer ------------------------------------------------------------

/// What kind of block we're currently inside. The renderer accumulates inline
/// content into `cur` and flushes it on the matching `End`.
enum Block {
    None,
    Paragraph,
    Heading(HeadingLevel),
    /// Buffered raw code text + detected language token.
    Code { lang: String, text: String },
    /// Block-quote: buffered like a paragraph but prefixed per visual line.
    Quote,
    /// A list item: inline text accumulates in `cur` after a leading marker span.
    ListItem,
    Table(TableBuf),
}

struct Renderer<'p> {
    palette: &'p Palette,
    width: usize,
    out: Vec<Vec<Span<'static>>>,
    /// Inline spans accumulated for the current text block (paragraph/heading/
    /// quote/table cell).
    cur: Vec<Span<'static>>,
    stack: Vec<Inline>,
    block: Block,
    /// List marker stack: `Some(n)` = ordered (next number), `None` = unordered.
    /// Depth = `lists.len()`.
    lists: Vec<Option<u64>>,
    /// Block-quote nesting depth. While `> 0`, paragraphs inside the quote do not
    /// start/flush their own block — the quote owns the buffered inline content.
    in_quote: u32,
    /// True once at least one block has been emitted (drives blank separators).
    started: bool,
    /// True once the current top-level list has emitted its leading separator, so
    /// items within one list stay tight (no blank line between them).
    list_sep_done: bool,
}

impl<'p> Renderer<'p> {
    fn new(palette: &'p Palette, width: usize) -> Self {
        Renderer {
            palette,
            width,
            out: Vec::new(),
            cur: Vec::new(),
            stack: Vec::new(),
            block: Block::None,
            lists: Vec::new(),
            in_quote: 0,
            started: false,
            list_sep_done: false,
        }
    }

    /// Emit a blank separator line before a new top-level block, except the very
    /// first. List items handle their own spacing, so the marker emits directly.
    fn sep(&mut self) {
        if self.started {
            self.out.push(vec![Span::raw("")]);
        }
        self.started = true;
    }

    /// Fold the inline stack into one effective [`Style`]. Inline code wins the
    /// colour slot (accent); link adds underline; the rest are modifiers.
    fn cur_style(&self) -> Style {
        let mut st = Style::default().fg(self.palette.fg);
        for inl in &self.stack {
            match inl {
                Inline::Bold => st = st.add_modifier(Modifier::BOLD),
                Inline::Italic => st = st.add_modifier(Modifier::ITALIC),
                Inline::Strike => st = st.add_modifier(Modifier::CROSSED_OUT),
                Inline::Link(_) => st = st.add_modifier(Modifier::UNDERLINED),
            }
        }
        st
    }

    /// Append a styled run of text to the current inline buffer.
    fn push_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        self.cur.push(Span::styled(t.to_string(), self.cur_style()));
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                // Inline code span: force accent regardless of surrounding stack.
                self.cur.push(Span::styled(
                    t.to_string(),
                    Style::default().fg(self.palette.accent),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                // Inside prose blocks a break becomes a new visual line; the
                // word-wrapper splits on the embedded newline.
                if let Block::Code { text, .. } = &mut self.block {
                    text.push('\n');
                } else {
                    self.cur.push(Span::raw("\n"));
                }
            }
            Event::Rule => {
                self.sep();
                self.out.push(vec![Span::styled(
                    "─".repeat(self.width),
                    Style::default().fg(self.palette.dim),
                )]);
            }
            // Lists/tasks beyond the handled set degrade to their text content,
            // which already arrives via Text events; nothing extra to do.
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if let Block::Code { text, .. } = &mut self.block {
            text.push_str(t);
        } else {
            self.push_text(t);
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                // A paragraph inside a list item or block-quote continues that
                // container's buffered content; only a standalone paragraph is its
                // own block. Consecutive paragraphs in one quote get a blank line.
                if self.in_quote > 0 {
                    if !self.cur.is_empty() {
                        self.cur.push(Span::raw("\n\n"));
                    }
                } else if self.lists.is_empty() {
                    self.block = Block::Paragraph;
                }
            }
            Tag::Heading { level, .. } => self.block = Block::Heading(level),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                self.block = Block::Code {
                    lang,
                    text: String::new(),
                };
            }
            Tag::BlockQuote(_) => {
                self.in_quote += 1;
                self.block = Block::Quote;
            }
            Tag::List(start) => {
                // A new top-level list is one block: arm its single leading
                // separator (nested lists inherit the parent's tight spacing).
                if self.lists.is_empty() {
                    self.list_sep_done = false;
                }
                self.lists.push(start);
            }
            Tag::Item => {
                // Render the marker line for this item immediately; the item's
                // inline text accumulates into `cur` and flushes on `End(Item)`.
                self.flush_cur_as_paragraph_if_pending();
                let depth = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                // Stash the prefix on `cur` as a leading dim span; the wrapper
                // hanging-indents continuations under it on flush.
                let prefix = format!("{indent}{marker}");
                self.cur.push(Span::styled(
                    prefix,
                    Style::default().fg(self.palette.dim),
                ));
                self.block = Block::ListItem;
            }
            Tag::Table(aligns) => {
                self.block = Block::Table(TableBuf {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    in_head: false,
                    cur_row: Vec::new(),
                });
            }
            Tag::TableHead => {
                if let Block::Table(tb) = &mut self.block {
                    tb.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Block::Table(tb) = &mut self.block {
                    tb.cur_row = Vec::new();
                }
            }
            Tag::TableCell => {
                // Cell inline content accumulates into `cur`; committed on
                // End(TableCell).
                self.cur = Vec::new();
            }
            Tag::Emphasis => self.stack.push(Inline::Italic),
            Tag::Strong => self.stack.push(Inline::Bold),
            Tag::Strikethrough => self.stack.push(Inline::Strike),
            Tag::Link { dest_url, .. } => self.stack.push(Inline::Link(dest_url.to_string())),
            Tag::Image { .. } => {
                // Images have no terminal rendering; their alt text flows through
                // as Text. Push a no-op inline so the matching End pops cleanly.
                self.stack.push(Inline::Italic);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                // Inside a list/quote the container flushes; standalone paragraphs
                // flush here.
                if self.in_quote == 0 && self.lists.is_empty() {
                    self.flush_paragraph();
                }
            }
            TagEnd::Heading(_) => self.flush_heading(),
            TagEnd::CodeBlock => self.flush_code(),
            TagEnd::BlockQuote(_) => {
                self.in_quote = self.in_quote.saturating_sub(1);
                // Only flush at the outermost level so nested quotes coalesce.
                if self.in_quote == 0 {
                    self.flush_quote();
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => self.flush_list_item(),
            TagEnd::Table => self.flush_table(),
            TagEnd::TableHead => {
                if let Block::Table(tb) = &mut self.block {
                    tb.in_head = false;
                    tb.head = std::mem::take(&mut tb.cur_row);
                }
            }
            TagEnd::TableRow => {
                if let Block::Table(tb) = &mut self.block {
                    if !tb.in_head {
                        let row = std::mem::take(&mut tb.cur_row);
                        tb.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.cur);
                if let Block::Table(tb) = &mut self.block {
                    tb.cur_row.push(cell);
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Image => {
                self.stack.pop();
            }
            TagEnd::Link => {
                // Append ` (url)` in dim after the link text, if any.
                if let Some(Inline::Link(url)) = self.stack.pop() {
                    if !url.is_empty() {
                        self.cur.push(Span::styled(
                            format!(" ({url})"),
                            Style::default().fg(self.palette.dim),
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    /// Defensive flush: if a previous list item's inline buffer is still pending
    /// when a new item starts (e.g. loose lists), emit it first.
    fn flush_cur_as_paragraph_if_pending(&mut self) {
        if matches!(self.block, Block::ListItem) && !self.cur.is_empty() {
            self.flush_list_item();
        }
    }

    // --- block flushers ------------------------------------------------------

    fn flush_paragraph(&mut self) {
        if self.cur.is_empty() {
            self.block = Block::None;
            return;
        }
        self.sep();
        let spans = std::mem::take(&mut self.cur);
        for vl in wrap_spans(&spans, self.width) {
            self.out.push(vl);
        }
        self.block = Block::None;
    }

    fn flush_heading(&mut self) {
        if self.cur.is_empty() {
            self.block = Block::None;
            return;
        }
        let level = match &self.block {
            Block::Heading(l) => *l,
            _ => HeadingLevel::H1,
        };
        // Levels 1-2 use the accent colour; 3-6 stay fg. All bold.
        let color = match level {
            HeadingLevel::H1 | HeadingLevel::H2 => self.palette.accent,
            _ => self.palette.fg,
        };
        // Restyle the accumulated spans to the heading colour + bold (headings
        // ignore inner emphasis colour, just force bold heading styling).
        let spans = std::mem::take(&mut self.cur);
        let restyled: Vec<Span<'static>> = spans
            .into_iter()
            .map(|s| {
                let m = s.style.add_modifier(Modifier::BOLD).fg(color);
                Span::styled(s.content.into_owned(), m)
            })
            .collect();
        self.sep();
        for vl in wrap_spans(&restyled, self.width) {
            self.out.push(vl);
        }
        self.block = Block::None;
    }

    fn flush_quote(&mut self) {
        if self.cur.is_empty() {
            self.block = Block::None;
            return;
        }
        let spans = std::mem::take(&mut self.cur);
        // Dim the quote body slightly and prefix each wrapped line with "│ ".
        let dimmed: Vec<Span<'static>> = spans
            .into_iter()
            .map(|s| {
                let st = s.style.fg(self.palette.dim);
                Span::styled(s.content.into_owned(), st)
            })
            .collect();
        let inner_w = self.width.saturating_sub(2).max(1);
        self.sep();
        for mut vl in wrap_spans(&dimmed, inner_w) {
            let mut line = vec![Span::styled(
                "│ ".to_string(),
                Style::default().fg(self.palette.dim),
            )];
            line.append(&mut vl);
            self.out.push(line);
        }
        self.block = Block::None;
    }

    fn flush_list_item(&mut self) {
        if self.cur.is_empty() {
            self.block = Block::None;
            return;
        }
        // One blank line before the list as a whole, then items stay tight.
        if !self.list_sep_done {
            self.sep();
            self.list_sep_done = true;
        }
        let depth = self.lists.len().saturating_sub(1);
        let indent = depth * 2;
        // The first span is the dim marker (indent+marker) we pushed on
        // Start(Item). Measure it to derive marker width + the hanging indent.
        let spans = std::mem::take(&mut self.cur);
        let marker_len = spans
            .first()
            .map(|s| s.content.chars().count())
            .unwrap_or(indent);
        // Wrap only the text spans (everything after the marker) to the width
        // left after the marker; then re-attach the marker to the first line and
        // hanging-indent the rest.
        let text_spans = &spans[1..];
        let avail = self.width.saturating_sub(marker_len).max(1);
        let wrapped = wrap_spans(text_spans, avail);
        let marker_span = spans[0].clone();
        let pad = " ".repeat(marker_len);
        for (i, mut vl) in wrapped.into_iter().enumerate() {
            let mut line = Vec::with_capacity(vl.len() + 1);
            if i == 0 {
                line.push(marker_span.clone());
            } else {
                line.push(Span::raw(pad.clone()));
            }
            line.append(&mut vl);
            self.out.push(line);
        }
        self.block = Block::None;
    }

    /// Draw a fenced/indented code block as a full box: `┌─ {lang} ───┐`, one
    /// padded content row per (possibly hard-split) source line with syntect
    /// colours, and `└────┘`. Indentation is preserved verbatim; lines wider
    /// than the inner width are hard-split at a char boundary.
    fn flush_code(&mut self) {
        let (lang, code) = match std::mem::replace(&mut self.block, Block::None) {
            Block::Code { lang, text } => (lang, text),
            _ => return,
        };
        self.sep();

        let dim = Style::default().fg(self.palette.dim);
        let w = self.width;
        let iw = w.saturating_sub(4); // "│ " + content + " │"

        // --- top border with optional language label ---
        let top = if lang.is_empty() {
            vec![Span::styled(
                format!("┌{}┐", "─".repeat(w.saturating_sub(2))),
                dim,
            )]
        } else {
            // Build: '┌' '─' ' lang ' then '─'*fill then '┐', totalling `w` cols.
            let label = format!(" {lang} ");
            let used = 1 /*┌*/ + 1 /*─*/ + label.chars().count();
            let fill = w.saturating_sub(used + 1 /*┐*/);
            vec![
                Span::styled("┌─".to_string(), dim),
                Span::styled(label, Style::default().fg(self.palette.accent)),
                Span::styled(format!("{}┐", "─".repeat(fill)), dim),
            ]
        };
        self.out.push(top);

        // --- content rows ---
        let syntax = syntaxes()
            .find_syntax_by_token(&lang)
            .unwrap_or_else(|| syntaxes().find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, theme());

        // Drop a single trailing newline so we don't emit a spurious blank row.
        let body = code.strip_suffix('\n').unwrap_or(&code);
        if body.is_empty() {
            // Empty code block: one blank content row keeps the box non-degenerate.
            self.out.push(self.code_row(Vec::new(), iw, dim));
        } else {
            for line in LinesWithEndings::from(body) {
                let ranges = hl
                    .highlight_line(line, syntaxes())
                    .unwrap_or_else(|_| vec![(syntect::highlighting::Style::default(), line)]);
                // Convert to (style, char-run) spans, stripping the line ending.
                let styled: Vec<(Style, String)> = ranges
                    .into_iter()
                    .map(|(s, txt)| {
                        let t = txt.trim_end_matches(['\n', '\r']).to_string();
                        (syntect_fg(s), t)
                    })
                    .filter(|(_, t)| !t.is_empty())
                    .collect();
                self.emit_code_line(&styled, iw, dim);
            }
        }

        // --- bottom border ---
        self.out.push(vec![Span::styled(
            format!("└{}┘", "─".repeat(w.saturating_sub(2))),
            dim,
        )]);
    }

    /// Build one code content row from already-styled spans whose total width is
    /// `<= iw`, padding with spaces to `iw` and wrapping in the box borders.
    fn code_row(&self, mut content: Vec<Span<'static>>, iw: usize, dim: Style) -> Vec<Span<'static>> {
        let used: usize = content.iter().map(|s| s.content.chars().count()).sum();
        let pad = iw.saturating_sub(used);
        if pad > 0 {
            content.push(Span::raw(" ".repeat(pad)));
        }
        let mut line = Vec::with_capacity(content.len() + 2);
        line.push(Span::styled("│ ".to_string(), dim));
        line.extend(content);
        line.push(Span::styled(" │".to_string(), dim));
        line
    }

    /// Emit a (possibly hard-split) source line as one or more boxed rows. The
    /// styled `(Style, String)` runs are walked char-by-char so a split lands on
    /// a char boundary at exactly `iw` columns, preserving all whitespace.
    fn emit_code_line(&mut self, styled: &[(Style, String)], iw: usize, dim: Style) {
        if iw == 0 {
            // Degenerate width: emit a single empty-bordered row and bail.
            self.out.push(self.code_row(Vec::new(), 0, dim));
            return;
        }
        // Flatten to (char, style) for exact-width chunking.
        let mut chars: Vec<(char, Style)> = Vec::new();
        for (st, s) in styled {
            for ch in s.chars() {
                chars.push((ch, *st));
            }
        }
        if chars.is_empty() {
            self.out.push(self.code_row(Vec::new(), iw, dim));
            return;
        }
        let mut i = 0;
        while i < chars.len() {
            let end = (i + iw).min(chars.len());
            let chunk = &chars[i..end];
            self.out.push(self.code_row(coalesce(chunk), iw, dim));
            i = end;
        }
    }

    /// Render a buffered GFM table: fit columns, then draw boxed rows honouring
    /// per-column alignment, header cells bold.
    fn flush_table(&mut self) {
        let tb = match std::mem::replace(&mut self.block, Block::None) {
            Block::Table(tb) => tb,
            _ => return,
        };
        let ncols = tb
            .head
            .len()
            .max(tb.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        self.sep();
        let dim = Style::default().fg(self.palette.dim);

        // Natural content width per column (max over header + body), in chars.
        let span_w = |cell: &[Span<'static>]| -> usize {
            cell.iter().map(|s| s.content.chars().count()).sum()
        };
        let mut widths = vec![0usize; ncols];
        for (c, cell) in tb.head.iter().enumerate() {
            widths[c] = widths[c].max(span_w(cell));
        }
        for row in &tb.rows {
            for (c, cell) in row.iter().enumerate() {
                widths[c] = widths[c].max(span_w(cell));
            }
        }
        // Each column is padded with one space on each side: "│ " + cell + " ".
        // Total box width = sum(widths) + 3*ncols + 1  (│ + ' '+w+' ' per col + │).
        let chrome = 3 * ncols + 1;
        let avail = self.width.saturating_sub(chrome);
        let total: usize = widths.iter().sum();
        if total > avail && avail >= ncols {
            // Shrink proportionally so the table fits, never below 1 col each.
            shrink_widths(&mut widths, avail);
        } else if total > avail {
            // Not even 1 char per column fits; clamp everything to 1.
            widths.fill(1);
        }

        let aligns = &tb.aligns;
        let align_of = |c: usize| aligns.get(c).copied().unwrap_or(Alignment::None);

        // Borders.
        let border = |left: &str, mid: &str, right: &str| -> Vec<Span<'static>> {
            let mut s = String::from(left);
            for (c, w) in widths.iter().enumerate() {
                if c > 0 {
                    s.push_str(mid);
                }
                // " " + w + " " worth of ─ = w + 2.
                s.push_str(&"─".repeat(w + 2));
            }
            s.push_str(right);
            vec![Span::styled(s, dim)]
        };

        // A data/header row: "│ " cell " │ " cell " │", cells truncated+aligned.
        let make_row =
            |cells: &[Vec<Span<'static>>], bold: bool, widths: &[usize], palette: &Palette| -> Vec<Span<'static>> {
                let mut line: Vec<Span<'static>> = Vec::new();
                for (c, w) in widths.iter().enumerate() {
                    line.push(Span::styled("│ ".to_string(), Style::default().fg(palette.dim)));
                    let empty: Vec<Span<'static>> = Vec::new();
                    let cell = cells.get(c).unwrap_or(&empty);
                    let mut padded = fit_cell(cell, *w, align_of(c), bold, palette);
                    line.append(&mut padded);
                    line.push(Span::raw(" "));
                }
                line.push(Span::styled("│".to_string(), Style::default().fg(palette.dim)));
                line
            };

        self.out.push(border("┌", "┬", "┐"));
        self.out.push(make_row(&tb.head, true, &widths, self.palette));
        self.out.push(border("├", "┼", "┤"));
        for row in &tb.rows {
            self.out.push(make_row(row, false, &widths, self.palette));
        }
        self.out.push(border("└", "┴", "┘"));
    }

    /// Flush any block left open by a malformed / truncated document, dispatching
    /// to the matching block flusher so a half-finished heading/quote/code/table
    /// still renders correctly (each flusher reads `self.block` + `self.cur`).
    fn finish(mut self) -> Vec<Vec<Span<'static>>> {
        match &self.block {
            Block::Heading(_) => self.flush_heading(),
            Block::Code { .. } => self.flush_code(),
            Block::Quote => self.flush_quote(),
            Block::ListItem => self.flush_list_item(),
            Block::Table(_) => self.flush_table(),
            // Paragraph or None: any pending inline content is a final paragraph.
            _ => self.flush_paragraph(),
        }
        self.out
    }
}

// --- helpers -----------------------------------------------------------------

/// Coalesce a run of `(char, Style)` into owned spans, merging adjacent chars
/// that share a style. Used to rebuild code-row spans after char-level chunking.
fn coalesce(run: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for &(ch, style) in run {
        match cur {
            Some(s) if s == style => buf.push(ch),
            _ => {
                if let Some(s) = cur {
                    line.push(Span::styled(std::mem::take(&mut buf), s));
                }
                buf.push(ch);
                cur = Some(style);
            }
        }
    }
    if let Some(s) = cur {
        line.push(Span::styled(buf, s));
    }
    line
}

/// Truncate a cell's inline spans to `w` chars (appending `…` when it overflows)
/// and pad to `w` honouring `align`. Header cells are forced bold.
fn fit_cell(
    cell: &[Span<'static>],
    w: usize,
    align: Alignment,
    bold: bool,
    palette: &Palette,
) -> Vec<Span<'static>> {
    let _ = palette;
    // Flatten to (char, style), truncate with an ellipsis if needed.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for s in cell {
        let st = if bold { s.style.add_modifier(Modifier::BOLD) } else { s.style };
        for ch in s.content.chars() {
            chars.push((ch, st));
        }
    }
    let len = chars.len();
    if len > w {
        if w == 0 {
            chars.clear();
        } else {
            chars.truncate(w.saturating_sub(1));
            let st = chars.last().map(|c| c.1).unwrap_or_default();
            chars.push(('…', st));
        }
    }
    let content_len = chars.len();
    let pad = w.saturating_sub(content_len);
    let (lpad, rpad) = match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        _ => (0, pad),
    };
    let mut out: Vec<Span<'static>> = Vec::new();
    if lpad > 0 {
        out.push(Span::raw(" ".repeat(lpad)));
    }
    out.extend(coalesce(&chars));
    if rpad > 0 {
        out.push(Span::raw(" ".repeat(rpad)));
    }
    out
}

/// Shrink `widths` so their sum equals `target`, taking from the widest columns
/// first and never reducing a column below 1.
fn shrink_widths(widths: &mut [usize], target: usize) {
    let ncols = widths.len();
    if ncols == 0 {
        return;
    }
    // Start from a floor of 1 per column; distribute the remaining budget
    // proportionally to each column's natural width.
    let total: usize = widths.iter().sum();
    if total == 0 || target <= ncols {
        widths.fill(1);
        return;
    }
    let spare = target - ncols; // budget above the 1-per-col floor
    let mut scaled: Vec<usize> = widths
        .iter()
        .map(|&w| 1 + (w.saturating_sub(1) * spare) / (total.saturating_sub(ncols)).max(1))
        .collect();
    // Fix rounding drift so the sum lands exactly on `target`.
    let mut sum: usize = scaled.iter().sum();
    while sum > target {
        // Trim the currently-widest column.
        if let Some((idx, _)) = scaled
            .iter()
            .enumerate()
            .filter(|(_, &w)| w > 1)
            .max_by_key(|(_, &w)| w)
        {
            scaled[idx] -= 1;
            sum -= 1;
        } else {
            break;
        }
    }
    while sum < target {
        // Pad the currently-narrowest column up toward its natural width.
        if let Some((idx, _)) = scaled
            .iter()
            .enumerate()
            .min_by_key(|(_, &w)| w)
        {
            scaled[idx] += 1;
            sum += 1;
        } else {
            break;
        }
    }
    widths.copy_from_slice(&scaled);
}

/// Word-wrap one logical line of styled `spans` to `width` columns, preserving
/// each span's [`Style`]. Returns visual lines (each a `Vec<Span>`). Counting is
/// in `char`s. Runs of non-whitespace are kept whole when they fit; words longer
/// than `width` are hard-split; `width` is clamped to `>= 1`. Embedded `\n`
/// chars force a hard break (soft/hard markdown breaks arrive as `\n`).
/// Consecutive same-style chars are coalesced back into one owned `Span` per run.
///
/// Shared by the chat view (`render_block`) and this module; `pub(crate)` so
/// there is a single implementation.
pub(crate) fn wrap_spans(spans: &[Span], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);

    // Flatten the styled spans into a single (char, style) sequence so wrapping
    // can break anywhere while remembering each char's style.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in spans {
        for ch in span.content.chars() {
            chars.push((ch, span.style));
        }
    }

    let mut out: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<(char, Style)> = Vec::new(); // chars committed to current visual line
    let mut word: Vec<(char, Style)> = Vec::new(); // current run of non-whitespace
    let mut line_len = 0usize;

    // Place the buffered `word` onto the current line, wrapping/splitting as
    // needed. A too-long word is hard-split across lines; otherwise it goes on
    // this line if it fits or starts a fresh one.
    let place_word = |out: &mut Vec<Vec<Span<'static>>>,
                      line: &mut Vec<(char, Style)>,
                      line_len: &mut usize,
                      word: &mut Vec<(char, Style)>| {
        if word.is_empty() {
            return;
        }
        let wlen = word.len();
        if wlen > width {
            if *line_len > 0 {
                out.push(coalesce(line));
                line.clear();
                *line_len = 0;
            }
            let mut start = 0usize;
            while word.len() - start > width {
                out.push(coalesce(&word[start..start + width]));
                start += width;
            }
            line.extend_from_slice(&word[start..]);
            *line_len = word.len() - start;
        } else if *line_len == 0 {
            line.extend_from_slice(word);
            *line_len = wlen;
        } else if *line_len + 1 + wlen <= width {
            line.push((' ', Style::default()));
            line.extend_from_slice(word);
            *line_len += 1 + wlen;
        } else {
            out.push(coalesce(line));
            line.clear();
            line.extend_from_slice(word);
            *line_len = wlen;
        }
        word.clear();
    };

    for &(ch, style) in &chars {
        if ch == '\n' {
            place_word(&mut out, &mut line, &mut line_len, &mut word);
            out.push(coalesce(&line));
            line.clear();
            line_len = 0;
        } else if ch.is_whitespace() {
            place_word(&mut out, &mut line, &mut line_len, &mut word);
        } else {
            word.push((ch, style));
        }
    }
    place_word(&mut out, &mut line, &mut line_len, &mut word);
    out.push(coalesce(&line));

    out
}
