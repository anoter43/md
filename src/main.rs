use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind};
use crossterm::execute;
use image::ImageReader;
use pulldown_cmark::{Alignment, CodeBlockKind, Event as MdEvent, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Styled};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_width::UnicodeWidthChar;

// ---------- kitty graphics protocol ----------

const KITTY_CHUNK: usize = 4096;

/// Build one APC escape: <ESC>_G<control>;<payload><ESC>\
fn kitty_esc(control: &str, payload: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(control.len() + payload.len() + 4);
    v.push(0x1b);
    v.extend_from_slice(b"_G");
    v.extend_from_slice(control.as_bytes());
    v.push(b';');
    v.extend_from_slice(payload.as_bytes());
    v.push(0x1b);
    v.push(b'\\');
    v
}

/// Transmit PNG data as image `id`, chunked into <=4096-char base64 pieces.
fn transmit_image(id: u32, png: &[u8]) -> Vec<u8> {
    let b64 = B64.encode(png);
    let mut out = Vec::new();
    let mut start = 0;
    let mut first = true;
    loop {
        let end = (start + KITTY_CHUNK).min(b64.len());
        let last = end == b64.len();
        let control = if first {
            format!("a=t,f=100,i={id},q=2,m={}", if last { 0 } else { 1 })
        } else {
            format!("m={}", if last { 0 } else { 1 })
        };
        out.extend(kitty_esc(&control, &b64[start..end]));
        if last {
            break;
        }
        start = end;
        first = false;
    }
    out
}

/// Place a previously transmitted image at a cell, sized `cw` x `ch` cells.
/// `src` is an optional source rectangle (image pixels) showing only a slice,
/// used when the image is partially scrolled off-screen.
fn place_image(id: u32, row: u16, col: u16, cw: u16, ch: u16, src: Option<(u32, u32, u32, u32)>) -> Vec<u8> {
    let mut out = format!("\x1b[{};{}H", row, col).into_bytes();
    let s = match src {
        Some((x0, y0, x1, y1)) => format!(",x={x0},y={y0},w={},h={}", x1 - x0, y1 - y0),
        None => String::new(),
    };
    out.extend(kitty_esc(&format!("a=p,i={id},p={id},c={cw},r={ch},q=2,C=1{s}"), ""));
    out
}

/// Remove an image from the screen, keeping its data cached for re-placing.
fn delete_image(id: u32) -> Vec<u8> {
    kitty_esc(&format!("a=d,d=i,i={id},q=2"), "")
}

/// Remove all images and free their data.
fn delete_all_images() -> Vec<u8> {
    kitty_esc("a=d,d=a,q=2", "")
}

fn decode_dims(path: &Path) -> Option<(u32, u32)> {
    let img = ImageReader::open(path).ok()?.decode().ok()?;
    Some((img.width(), img.height()))
}

/// Decode any supported image, downscale it to at most `max_px` on the longest
/// side (terminal cells are ~8px wide, so full resolution is wasted data), and
/// re-encode it as PNG for transmission. Returns the PNG plus its pixel dims.
fn encode_png(path: &Path, max_px: u32) -> Option<(Vec<u8>, u32, u32)> {
    let img = ImageReader::open(path).ok()?.decode().ok()?;
    let img = if img.width() > max_px || img.height() > max_px {
        img.thumbnail(max_px, max_px)
    } else {
        img
    };
    let dims = (img.width(), img.height());
    let mut buf = Vec::new();
    img.write_to(&mut io::Cursor::new(&mut buf), image::ImageFormat::Png).ok()?;
    Some((buf, dims.0, dims.1))
}

// ---------- markdown -> styled lines ----------

enum InlineStyle {
    Emphasis,
    Strong,
    Strike,
    Link { url: Option<String>, start: usize },
    Image { url: Option<String>, alt: String },
}

struct ListCtx {
    ordered: bool,
    number: u64,
}

/// A heading found in the document, for the outline.
struct Heading {
    title: String,
    level: u8,
    /// Index into the (unwrapped) content lines.
    content_line: usize,
    /// Display line after wrapping, kept in sync by `reflow`.
    line: usize,
}

/// A markdown image, rendered via the kitty graphics protocol.
struct Image {
    id: u32,
    path: PathBuf,
    content_line: usize,
    /// Pixel dimensions if the file could be decoded.
    dims: Option<(u32, u32)>,
    /// Cell size computed during reflow (0 if undecodable).
    cw: u16,
    ch: u16,
    /// Encoded PNG ready for transmission, cached after first encode.
    png: Option<Vec<u8>>,
    /// Pixel dims of the cached PNG (for source-rect clipping when partially visible).
    png_dims: Option<(u32, u32)>,
}

/// A table collected during rendering; laid out in `reflow` once the width is known.
struct Table {
    /// Index of the marker line in `content` that `reflow` expands.
    content_line: usize,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    header_rows: usize,
    align: Vec<Alignment>,
}

/// A table being collected while parsing.
struct TableBuf {
    rows: Vec<Vec<Vec<Span<'static>>>>,
    header_rows: usize,
    align: Vec<Alignment>,
}

fn muted() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn heading_color(level: u8) -> Color {
    match level {
        1 => Color::Yellow,
        2 => Color::Green,
        _ => Color::Cyan,
    }
}

/// Builds styled terminal lines from a stream of markdown events.
struct Md {
    out: Vec<Line<'static>>,
    headings: Vec<Heading>,
    images: Vec<Image>,
    tables: Vec<Table>,
    next_image_id: u32,
    /// Directory the markdown file lives in; relative image paths resolve against it.
    base: PathBuf,
    /// Inline content of the current paragraph / heading / table cell.
    buf: Vec<Span<'static>>,
    /// Open inline styling contexts (emphasis, strong, link, ...).
    styles: Vec<InlineStyle>,
    quote: usize,
    list: Vec<ListCtx>,
    item_prefix_used: bool,
    item_number: Option<u64>,
    heading: Option<u8>,
    in_code: bool,
    code: String,
    code_lang: Option<String>,
    table: Option<TableBuf>,
    table_row: Vec<Vec<Span<'static>>>,
    cell: Option<Vec<Span<'static>>>,
}

impl Md {
    fn new(base: PathBuf) -> Self {
        Md {
            out: Vec::new(),
            headings: Vec::new(),
            images: Vec::new(),
            tables: Vec::new(),
            next_image_id: 1,
            base,
            buf: Vec::new(),
            styles: Vec::new(),
            quote: 0,
            list: Vec::new(),
            item_prefix_used: false,
            item_number: None,
            heading: None,
            in_code: false,
            code: String::new(),
            code_lang: None,
            table: None,
            table_row: Vec::new(),
            cell: None,
        }
    }

    /// Where inline content currently goes: the open table cell, else the paragraph buffer.
    fn target(&mut self) -> &mut Vec<Span<'static>> {
        self.cell.as_mut().unwrap_or(&mut self.buf)
    }

    fn push_span(&mut self, span: Span<'static>) {
        self.target().push(span);
    }

    fn style_now(&self) -> Style {
        let mut s = Style::default();
        for f in &self.styles {
            match f {
                InlineStyle::Emphasis => s = s.add_modifier(Modifier::ITALIC),
                InlineStyle::Strong => s = s.add_modifier(Modifier::BOLD),
                InlineStyle::Strike => s = s.add_modifier(Modifier::CROSSED_OUT),
                InlineStyle::Link { .. } => s = s.fg(Color::Cyan).underlined(),
                InlineStyle::Image { .. } => {}
            }
        }
        s
    }

    /// Push a blank separator line, unless we're at the start or already blank.
    fn blank(&mut self) {
        let last_empty = self.out.last().is_none_or(|l| l.spans.is_empty());
        if !self.out.is_empty() && !last_empty {
            self.out.push(Line::default());
        }
    }

    /// Emit the pending inline buffer as one line, adding quote and list prefixes.
    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        if self.quote > 0 {
            spans.push(Span::raw("▎ ".repeat(self.quote)).style(muted()));
        }
        if !self.item_prefix_used
            && let Some(ctx) = self.list.last()
        {
            let indent = "  ".repeat(self.list.len().saturating_sub(1));
            let marker = if ctx.ordered {
                format!("{}. ", self.item_number.unwrap_or(ctx.number))
            } else {
                "• ".to_string()
            };
            spans.push(Span::raw(format!("{indent}{marker}")).style(muted()));
            self.item_prefix_used = true;
        }
        spans.append(&mut self.buf);
        let line = Line::from(spans);
        if self.quote > 0 {
            self.out.push(line.style(Color::Gray));
        } else {
            self.out.push(line);
        }
    }

    fn text(&mut self, t: &str) {
        if self.in_code {
            self.code.push_str(t);
            return;
        }
        if let Some(InlineStyle::Image { alt, .. }) = self.styles.last_mut() {
            alt.push_str(t); // image alt text
            return;
        }
        let s = self.style_now();
        self.push_span(Span::raw(t.to_string()).style(s));
    }

    fn flush_code(&mut self) {
        if let Some(lang) = self.code_lang.take() {
            self.out.push(Line::styled(
                format!("  {lang}"),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ));
        }
        let text = self.code.trim_end_matches('\n');
        for line in text.split('\n') {
            self.out.push(Line::styled(format!("  {line}"), muted()));
        }
        self.code.clear();
    }

    fn item_start(&mut self) {
        self.flush();
        self.item_prefix_used = false;
        if let Some(ctx) = self.list.last_mut() {
            if ctx.ordered {
                self.item_number = Some(ctx.number);
                ctx.number += 1;
            } else {
                self.item_number = None;
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                self.blank();
                self.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::List(start) => {
                self.flush();
                self.blank();
                self.list.push(ListCtx {
                    ordered: start.is_some(),
                    number: start.unwrap_or(1),
                });
            }
            Tag::Item => self.item_start(),
            Tag::CodeBlock(kind) => {
                self.flush();
                self.blank();
                self.in_code = true;
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
            }
            Tag::Emphasis => self.styles.push(InlineStyle::Emphasis),
            Tag::Strong => self.styles.push(InlineStyle::Strong),
            Tag::Strikethrough => self.styles.push(InlineStyle::Strike),
            Tag::Link { dest_url, .. } => {
                let start = self.target().len();
                self.styles.push(InlineStyle::Link {
                    url: Some(dest_url.to_string()),
                    start,
                });
            }
            Tag::Image { dest_url, .. } => self.styles.push(InlineStyle::Image {
                url: Some(dest_url.to_string()),
                alt: String::new(),
            }),
            Tag::Table(align) => {
                self.flush();
                self.blank();
                self.table = Some(TableBuf {
                    rows: Vec::new(),
                    header_rows: 0,
                    align,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                self.table_row = Vec::new();
            }
            Tag::TableCell => self.cell = Some(Vec::new()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(level) => {
                if self.heading.is_some() {
                    if !self.buf.is_empty() {
                        let title: String =
                            self.buf.iter().map(|s| s.content.as_ref()).collect();
                        let content_line = self.out.len();
                        self.out.push(
                            Line::from(std::mem::take(&mut self.buf)).style(
                                Style::default()
                                    .bold()
                                    .fg(heading_color(level as u8)),
                            ),
                        );
                        self.headings.push(Heading {
                            title,
                            level: level as u8,
                            content_line,
                            line: content_line,
                        });
                        self.blank();
                    }
                    self.heading = None;
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.list.pop();
                self.blank();
            }
            TagEnd::Item => self.flush(),
            TagEnd::CodeBlock => {
                self.flush_code();
                self.in_code = false;
                self.blank();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Image => {
                if let Some(InlineStyle::Image { url: Some(url), alt }) = self.styles.pop() {
                    let raw = PathBuf::from(&url);
                    let path = if raw.is_absolute() {
                        raw
                    } else {
                        self.base.join(&raw)
                    };
                    let label = if alt.is_empty() {
                        path.file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "image".to_string())
                    } else {
                        alt.clone()
                    };
                    let content_line = self.out.len();
                    let dims = decode_dims(&path);
                    // A dim placeholder line; the real image is drawn on top by kitty.
                    self.out.push(Line::styled(format!("▦ {label}"), muted()));
                    self.images.push(Image {
                        id: self.next_image_id,
                        path,
                        content_line,
                        dims,
                        cw: 0,
                        ch: 0,
                        png: None,
                        png_dims: None,
                    });
                    self.next_image_id += 1;
                }
            }
            TagEnd::Link => {
                if let Some(InlineStyle::Link { url: Some(url), start }) = self.styles.pop() {
                    let text: String = self.target()[start..]
                        .iter()
                        .map(|s| s.content.as_ref().to_string())
                        .collect();
                    if text != url {
                        self.push_span(Span::raw(format!(" ({url})")).style(muted()));
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take()
                    && !table.rows.is_empty()
                {
                    let content_line = self.out.len();
                    // Marker line; `reflow` replaces it with the laid-out table.
                    self.out.push(Line::default());
                    self.tables.push(Table {
                        content_line,
                        rows: table.rows,
                        header_rows: table.header_rows,
                        align: table.align,
                    });
                }
                self.blank();
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if !self.table_row.is_empty()
                    && let Some(table) = self.table.as_mut()
                {
                    table.rows.push(std::mem::take(&mut self.table_row));
                    if matches!(tag, TagEnd::TableHead) {
                        table.header_rows += 1;
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(cell) = self.cell.take() {
                    self.table_row.push(cell);
                }
            }
            _ => {}
        }
    }

    fn event(&mut self, ev: MdEvent) {
        match ev {
            MdEvent::Start(tag) => self.start(tag),
            MdEvent::End(tag) => self.end(tag),
            MdEvent::Text(t) => self.text(&t),
            MdEvent::Code(c) => self.push_span(Span::raw(c.to_string()).style(muted())),
            MdEvent::SoftBreak => self.push_span(Span::raw(" ")),
            MdEvent::HardBreak => self.flush(),
            MdEvent::Rule => {
                self.flush();
                self.out.push(Line::styled("─".repeat(100), muted()));
                self.blank();
            }
            MdEvent::TaskListMarker(checked) => {
                let span = if checked {
                    Span::raw("[x] ").style(Style::default().fg(Color::Green))
                } else {
                    Span::raw("[ ] ").style(muted())
                };
                self.push_span(span);
            }
            _ => {}
        }
    }
}

fn render(
    md_text: &str,
    base: &Path,
) -> (Vec<Line<'static>>, Vec<Heading>, Vec<Image>, Vec<Table>) {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut m = Md::new(base.to_path_buf());
    for ev in Parser::new_ext(md_text, opts) {
        m.event(ev);
    }
    m.flush();
    (m.out, m.headings, m.images, m.tables)
}

/// Lay out a table to fit `width` columns: shrink the widest columns and
/// truncate overflowing cells with an ellipsis, so the borders never wrap.
fn render_table(table: &Table, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let rows = &table.rows;
    let cols = table
        .align
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let w: usize = cell.iter().map(|s| s.width()).sum();
            widths[c] = widths[c].max(w);
        }
    }
    // Shrink the widest column, one cell at a time, until the table fits.
    let border_chars = 3 * cols + 1; // padding + separators + outer borders
    let budget = width.saturating_sub(border_chars);
    if budget > 0 {
        let mut total: usize = widths.iter().sum();
        while total > budget {
            let Some(idx) = (0..cols)
                .max_by_key(|&c| widths[c])
                .filter(|&c| widths[c] > 2)
            else {
                break;
            };
            widths[idx] -= 1;
            total -= 1;
        }
    }

    let border = |l: char, m: char, r: char| {
        let mut s = String::from(l);
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push(if i + 1 == widths.len() { r } else { m });
        }
        Line::styled(s, muted())
    };

    // Wrap every cell to its column width, so long content stays visible
    // without ever breaking the table's borders.
    let col_w = |c: usize| widths[c];
    let mut wrapped: Vec<Vec<Vec<Line<'static>>>> = Vec::with_capacity(rows.len());
    let mut heights: Vec<usize> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cell_lines = Vec::with_capacity(cols);
        let mut h = 1usize;
        for c in 0..cols {
            let cell = row.get(c).map(Vec::as_slice).unwrap_or(&[]);
            let align = table.align.get(c).copied().unwrap_or(Alignment::None);
            let cl = wrap_cell(cell, col_w(c), align);
            h = h.max(cl.len());
            cell_lines.push(cl);
        }
        heights.push(h);
        wrapped.push(cell_lines);
    }
    // Pad every cell to the row height so all cells in a row share the same
    // number of visual lines and the vertical borders stay aligned.
    for ri in 0..rows.len() {
        for (c, cell_lines) in wrapped[ri].iter_mut().enumerate() {
            while cell_lines.len() < heights[ri] {
                cell_lines.push(Line::from(vec![Span::raw(" ".repeat(col_w(c)))]));
            }
        }
    }
    // A full grid (separators between every row) once any row wraps, so
    // wrapped rows stay visually distinct from their neighbors.
    let full_grid = heights.iter().any(|&h| h > 1);

    lines.push(border('┌', '┬', '┐'));
    for ri in 0..rows.len() {
        for vi in 0..heights[ri] {
            let mut spans: Vec<Span<'static>> = vec![Span::raw("│ ").style(muted())];
            for cell_lines in &wrapped[ri] {
                let cell_line = cell_lines.get(vi).cloned().unwrap_or_default();
                spans.extend(cell_line.spans);
                spans.push(Span::raw(" │ ").style(muted()));
            }
            spans.pop();
            spans.push(Span::raw(" │").style(muted()));
            let line = Line::from(spans);
            if ri < table.header_rows {
                lines.push(line.style(Style::default().bold()));
            } else {
                lines.push(line);
            }
        }
        let sep_after = if full_grid {
            ri + 1 < rows.len()
        } else {
            ri + 1 == table.header_rows && ri + 1 < rows.len()
        };
        if sep_after {
            lines.push(border('├', '┼', '┤'));
        }
    }
    lines.push(border('└', '┴', '┘'));
    lines
}

/// Wrap a table cell to `col_w` columns, padding each wrapped line to the
/// full column width so the vertical borders stay aligned.
fn wrap_cell(cell: &[Span<'static>], col_w: usize, align: Alignment) -> Vec<Line<'static>> {
    let mut wrapped = wrap_line(&Line::from(cell.to_vec()), col_w);
    for line in &mut wrapped {
        let w: usize = line.spans.iter().map(|s| s.width()).sum();
        let pad = col_w.saturating_sub(w);
        if pad == 0 {
            continue;
        }
        let (left, right) = match align {
            Alignment::Center => (pad / 2, pad - pad / 2),
            Alignment::Right => (pad, 0),
            _ => (0, pad),
        };
        let mut spans = Vec::with_capacity(line.spans.len() + 2);
        if left > 0 {
            spans.push(Span::raw(" ".repeat(left)));
        }
        spans.append(&mut line.spans);
        if right > 0 {
            spans.push(Span::raw(" ".repeat(right)));
        }
        line.spans = spans;
    }
    wrapped
}

// ---------- line wrapping ----------

/// Wrap a single styled line to `width` columns at word boundaries
/// (hard-breaking words longer than the width). Preserves span styles.
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.spans.is_empty() {
        return vec![Line::default()];
    }
    if width == 0 {
        return vec![line.clone()];
    }
    let base = line.style;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut pending = 0usize; // uncommitted spaces at the end of the current line

    for span in &line.spans {
        let style = span.style;
        let mut word = String::new();
        let mut word_w = 0usize;
        for ch in span.content.as_ref().chars() {
            if ch.is_whitespace() {
                if !word.is_empty() {
                    push_word(
                        &word, word_w, style, base, width, &mut out, &mut cur, &mut cur_w,
                        &mut pending,
                    );
                    word.clear();
                    word_w = 0;
                }
                if !cur.is_empty() {
                    pending += 1;
                }
            } else {
                word.push(ch);
                word_w += ch.width().unwrap_or(0);
            }
        }
        if !word.is_empty() {
            push_word(
                &word, word_w, style, base, width, &mut out, &mut cur, &mut cur_w, &mut pending,
            );
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur).style(base));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn push_word(
    word: &str,
    w: usize,
    style: Style,
    base: Style,
    width: usize,
    out: &mut Vec<Line<'static>>,
    cur: &mut Vec<Span<'static>>,
    cur_w: &mut usize,
    pending: &mut usize,
) {
    if w == 0 {
        return;
    }
    if *cur_w + *pending + w <= width {
        if *pending > 0 {
            cur.push(Span::raw(" ".repeat(*pending)));
            *cur_w += *pending;
            *pending = 0;
        }
        cur.push(Span::raw(word.to_string()).style(style));
        *cur_w += w;
        return;
    }
    // Doesn't fit on the current line: start a new one.
    if !cur.is_empty() {
        out.push(Line::from(std::mem::take(cur)).style(base));
        *cur_w = 0;
        *pending = 0;
    }
    if w <= width {
        cur.push(Span::raw(word.to_string()).style(style));
        *cur_w = w;
        return;
    }
    // A single word wider than the line: hard-break it.
    let mut chunk = String::new();
    let mut chunk_w = 0usize;
    for ch in word.chars() {
        let cw = ch.width().unwrap_or(0);
        if chunk_w + cw > width && !chunk.is_empty() {
            cur.push(Span::raw(std::mem::take(&mut chunk)).style(style));
            *cur_w += chunk_w;
            chunk_w = 0;
            out.push(Line::from(std::mem::take(cur)).style(base));
            *cur_w = 0;
        }
        chunk.push(ch);
        chunk_w += cw;
    }
    if !chunk.is_empty() {
        cur.push(Span::raw(chunk).style(style));
        *cur_w += chunk_w;
    }
}

// ---------- TUI ----------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Reader,
    Search,
    Picker,
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

struct Picker {
    cwd: PathBuf,
    start: PathBuf,
    entries: Vec<Entry>,
    selected: usize,
    from_reader: bool,
    show_hidden: bool,
}

impl Picker {
    fn new(cwd: PathBuf) -> Self {
        let start = cwd.clone();
        Picker {
            cwd,
            start,
            entries: Vec::new(),
            selected: 0,
            from_reader: false,
            show_hidden: false,
        }
    }
}

struct App {
    /// Logical (unwrapped) markdown lines.
    content: Vec<Line<'static>>,
    /// Wrapped display lines.
    lines: Vec<Line<'static>>,
    headings: Vec<Heading>,
    images: Vec<Image>,
    tables: Vec<Table>,
    /// (image index, display row) pairs filled in by `reflow`.
    image_rows: Vec<(usize, usize)>,
    /// Whether each image was transmitted+placed in the previous frame.
    image_placed: Vec<bool>,
    /// Width used for the current wrapping; usize::MAX until first frame.
    wrap_width: usize,
    offset: usize,
    viewport: usize,
    show_outline: bool,
    selected: usize,
    filename: String,
    /// Full path of the open file (used by the editor shortcut and reload).
    path: PathBuf,
    mode: Mode,
    query: String,
    matches: Vec<usize>,
    picker: Picker,
    /// Content area origin/size (for placing images in screen coordinates).
    content_x: u16,
    content_y: u16,
    content_w: u16,
    /// Transient message shown in the status bar (e.g. editor failures).
    status_msg: Option<String>,
}

impl App {
    fn reader(
        content: Vec<Line<'static>>,
        headings: Vec<Heading>,
        filename: String,
        images: Vec<Image>,
        tables: Vec<Table>,
    ) -> App {
        let n = images.len();
        App {
            content,
            lines: Vec::new(),
            headings,
            images,
            tables,
            image_rows: Vec::new(),
            image_placed: vec![false; n],
            wrap_width: usize::MAX,
            offset: 0,
            viewport: 0,
            show_outline: true,
            selected: 0,
            filename,
            path: PathBuf::from("."),
            mode: Mode::Reader,
            query: String::new(),
            matches: Vec::new(),
            picker: Picker::new(PathBuf::from(".")),
            content_x: 0,
            content_y: 0,
            content_w: 0,
            status_msg: None,
        }
    }
}

fn image_cell_size(img: &Image, width: usize) -> (u16, u16) {
    let Some((w, h)) = img.dims else {
        return (0, 0);
    };
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let max_w = (width.saturating_sub(4)).clamp(10, 60) as u32;
    let cw = max_w.min(w); // don't upscale beyond native width
    // Terminal cells are roughly twice as tall as wide.
    let ch = ((cw as f64 * h as f64 / w as f64) / 2.0).round() as u32;
    (cw as u16, ch.clamp(3, 30) as u16)
}

fn reflow(app: &mut App, width: usize) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut offsets = Vec::with_capacity(app.content.len());
    app.image_rows.clear();
    let mut img_idx = 0usize;
    let mut tbl_idx = 0usize;
    for (i, line) in app.content.iter().enumerate() {
        offsets.push(lines.len());
        if img_idx < app.images.len() && app.images[img_idx].content_line == i {
            let (cw, ch) = image_cell_size(&app.images[img_idx], width);
            app.images[img_idx].cw = cw;
            app.images[img_idx].ch = ch;
            app.image_rows.push((img_idx, lines.len()));
            lines.extend(wrap_line(line, width));
            // Reserve vertical space matching the image's cell height.
            for _ in 1..ch {
                lines.push(Line::default());
            }
            img_idx += 1;
        } else if tbl_idx < app.tables.len() && app.tables[tbl_idx].content_line == i {
            lines.extend(render_table(&app.tables[tbl_idx], width));
            tbl_idx += 1;
        } else {
            lines.extend(wrap_line(line, width));
        }
    }
    app.lines = lines;
    for h in &mut app.headings {
        h.line = offsets[h.content_line];
    }
}

fn select_heading(app: &mut App, idx: usize) {
    if app.headings.is_empty() {
        return;
    }
    app.selected = idx.min(app.headings.len() - 1);
    app.offset = app.headings[app.selected].line;
}

fn toggle_outline(app: &mut App) {
    app.show_outline = !app.show_outline;
    if app.show_outline {
        app.selected = app
            .headings
            .iter()
            .rposition(|h| h.line <= app.offset)
            .unwrap_or(0);
    }
}

fn load_file(app: &mut App, path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let base = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let (content, headings, images, tables) = render(&text, &base);
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    app.content = content;
    app.headings = headings;
    app.images = images;
    app.tables = tables;
    app.image_placed = vec![false; app.images.len()];
    app.image_rows.clear();
    app.lines = Vec::new();
    app.wrap_width = usize::MAX;
    app.offset = 0;
    app.viewport = 0;
    app.show_outline = true;
    app.selected = 0;
    app.filename = filename;
    app.path = path.to_path_buf();
    app.mode = Mode::Reader;
    let _ = io::stdout().write_all(&delete_all_images());
    let _ = io::stdout().flush();
    true
}

/// Re-read the open file after editing: keep the scroll position, refresh everything.
fn reload(app: &mut App) {
    let Ok(text) = std::fs::read_to_string(&app.path) else {
        return;
    };
    let base = app
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let (content, headings, images, tables) = render(&text, &base);
    let filename = app
        .path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.path.display().to_string());
    app.content = content;
    app.headings = headings;
    app.images = images;
    app.tables = tables;
    app.image_placed = vec![false; app.images.len()];
    app.image_rows.clear();
    app.lines = Vec::new();
    app.wrap_width = usize::MAX; // force a reflow (which clamps the offset)
    app.selected = app.selected.min(app.headings.len().saturating_sub(1));
    app.filename = filename;
    let _ = io::stdout().write_all(&delete_all_images());
    let _ = io::stdout().flush();
}

/// True if `prog` resolves to an existing executable on `$PATH` (or a
/// path-relative/absolute location).
fn in_path(prog: &str) -> bool {
    if prog.contains('/') {
        return std::fs::metadata(prog).map(|m| m.is_file()).unwrap_or(false);
    }
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path).find_map(|dir| {
                let full = dir.join(prog);
                std::fs::metadata(&full).map(|m| m.is_file()).ok()
            })
        })
        .is_some()
}

/// Build the editor command: `$VISUAL`, then `$EDITOR`, then a fallback list
/// of common editors (first one present on `$PATH`).
fn find_editor() -> Option<std::process::Command> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(spec) = std::env::var(var) {
            let mut parts = spec.split_whitespace();
            if let Some(prog) = parts.next()
                && in_path(prog)
            {
                let mut cmd = std::process::Command::new(prog);
                cmd.args(parts);
                return Some(cmd);
            }
        }
    }
    for spec in [
        "nvim", "vim", "vi", "nano", "emacs", "micro", "code -w", "subl -w",
        "kate", "gedit",
    ] {
        let mut parts = spec.split_whitespace();
        if let Some(prog) = parts.next()
            && in_path(prog)
        {
            let mut cmd = std::process::Command::new(prog);
            cmd.args(parts);
            return Some(cmd);
        }
    }
    None
}

fn refresh_picker(app: &mut App) {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&app.picker.cwd) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !app.picker.show_hidden && name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push(Entry {
                name,
                path: entry.path(),
                is_dir,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    app.picker.entries = entries;
    app.picker.selected = 0;
}

/// Transmit + place visible images via the kitty protocol; delete scrolled-off ones.
fn place_images(app: &mut App) -> io::Result<()> {
    let mut out = io::stdout();
    let mut visible = vec![false; app.images.len()];
    let vp = app.viewport;
    for &(idx, row) in &app.image_rows {
        let img = &app.images[idx];
        if img.cw == 0 || img.ch == 0 {
            continue;
        }
        // The image spans rows [row, row+ch); keep it while it overlaps the viewport.
        let ch_cells = img.ch as usize;
        let lo = row.max(app.offset);
        let hi = (row + ch_cells).min(app.offset + vp);
        if lo >= hi {
            continue;
        }
        visible[idx] = true;
        let screen_row = app.content_y as usize + (lo - app.offset) + 1;
        let inner_w = app.content_w as usize;
        let col = app.content_x as usize + 1 + inner_w.saturating_sub(img.cw as usize) / 2;
        if !app.image_placed[idx] {
            let img = &mut app.images[idx];
            // Encode once, then reuse the cached PNG on scroll-back.
            if img.png.is_none() {
                // Cells are ~8px wide, so cap the longest side at 8x the width.
                let max_px = (img.cw as u32).saturating_mul(8).max(32);
                match encode_png(&img.path, max_px) {
                    Some((png, w, h)) => {
                        img.png_dims = Some((w, h));
                        img.png = Some(png);
                    }
                    None => continue,
                }
            }
            let img = &app.images[idx];
            out.write_all(&transmit_image(img.id, img.png.as_ref().unwrap()))?;
        }
        let img = &app.images[idx];
        // Show only the visible slice when the image is clipped by the viewport.
        let (w, h) = img.png_dims.unwrap_or((0, 0));
        let src = if h > 0 && (lo != row || hi != row + ch_cells) {
            let y0 = (h as usize * (lo - row)) / ch_cells;
            let y1 = (h as usize * (hi - row)) / ch_cells;
            Some((0, y0 as u32, w, y1 as u32))
        } else {
            None
        };
        out.write_all(&place_image(
            img.id,
            screen_row as u16,
            col as u16,
            img.cw,
            (hi - lo) as u16,
            src,
        ))?;
    }
    for ((img, was_placed), is_visible) in app.images.iter().zip(&app.image_placed).zip(&visible) {
        if *was_placed && !*is_visible {
            out.write_all(&delete_image(img.id))?;
        }
    }
    app.image_placed = visible;
    out.flush()
}

fn render_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(Span::styled(" Outline ", Style::default().bold()));
    let inner = block.clone().inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line<'static>> = app
        .headings
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let indent = "  ".repeat(h.level.saturating_sub(1) as usize);
            let selected = i == app.selected;
            let spans = vec![
                Span::raw(if selected { "▸ " } else { "  " }).style(if selected {
                    Style::default().fg(Color::Yellow)
                } else {
                    muted()
                }),
                Span::raw(format!("{indent}{}", h.title))
                    .style(Style::default().fg(heading_color(h.level))),
            ];
            let mut line = Line::from(spans);
            if selected {
                line = line.style(Style::default().bg(Color::DarkGray));
            }
            line
        })
        .collect();

    let max = lines.len().saturating_sub(inner.height as usize);
    let sb_offset = app.selected.saturating_sub(inner.height as usize / 2).min(max);
    frame.render_widget(
        Paragraph::new(lines).scroll((sb_offset.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}

fn ui_picker(frame: &mut Frame, app: &mut App) {
    let vert = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(frame.area());
    let list_area = vert[0];
    let status_area = vert[1];

    let block = Block::bordered().title(Span::styled(
        format!(" {} ", app.picker.cwd.display()),
        Style::default().bold(),
    ));
    let inner = block.clone().inner(list_area);
    frame.render_widget(block, list_area);

    let items: Vec<ListItem<'static>> = app
        .picker
        .entries
        .iter()
        .map(|e| {
            let span = if e.is_dir {
                Span::styled(format!("▸ {}/", e.name), Style::default().fg(Color::Cyan))
            } else if e.name.ends_with(".md") || e.name.ends_with(".markdown") {
                Span::styled(e.name.clone(), Style::default().fg(Color::Green))
            } else {
                Span::raw(e.name.clone())
            };
            ListItem::new(Line::from(vec![span]))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray))
        .scroll_padding(2);
    let mut state = ListState::default().with_selected(Some(app.picker.selected));
    frame.render_stateful_widget(list, inner, &mut state);

    let keys = format!(
        "↑/↓ j/k select · Enter open · Backspace/Esc up · h hidden:{} · q quit",
        if app.picker.show_hidden { "on" } else { "off" }
    );
    frame.render_widget(
        Paragraph::new(keys).style(Style::default().fg(Color::White).bg(Color::DarkGray)),
        status_area,
    );
}

fn ui(frame: &mut Frame, app: &mut App) {
    if app.mode == Mode::Picker {
        ui_picker(frame, app);
        return;
    }

    let vert = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(frame.area());
    let body = vert[0];
    let status_area = vert[1];

    let (content_area, sidebar_area) = if app.show_outline && !app.headings.is_empty() {
        let hor = Layout::horizontal([Constraint::Length(24), Constraint::Min(0)]).split(body);
        (hor[1], Some(hor[0]))
    } else {
        (body, None)
    };
    if let Some(area) = sidebar_area {
        render_sidebar(frame, app, area);
    }

    let block = Block::bordered().title(Span::styled(
        format!(" {} ", app.filename),
        Style::default().bold(),
    ));
    let inner = block.clone().inner(content_area);
    frame.render_widget(block, content_area);

    app.content_x = inner.x;
    app.content_y = inner.y;
    app.content_w = inner.width;

    // Leave one column for the scrollbar.
    let width = inner.width.saturating_sub(1) as usize;
    if width != app.wrap_width {
        reflow(app, width);
        app.wrap_width = width;
    }

    let viewport = inner.height as usize;
    app.viewport = viewport;
    app.offset = app.offset.min(app.lines.len().saturating_sub(viewport));

    // Build display lines, highlighting search matches if a query is active.
    let mut display = app.lines.clone();
    if app.mode == Mode::Search && !app.query.is_empty() {
        app.matches.clear();
        let needle = app.query.to_lowercase();
        for (i, line) in app.lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if text.to_lowercase().contains(&needle) {
                app.matches.push(i);
                display[i] = display[i].clone().set_style(Style::default().bg(Color::Yellow));
            }
        }
    }

    let para = Paragraph::new(display).scroll((
        app.offset.min(u16::MAX as usize) as u16,
        0,
    ));
    frame.render_widget(para, inner);

    // Ratatui assumes `position` ranges over 0..=content_length-1, so pass the
    // number of scroll offsets (not the raw line count); otherwise the thumb
    // stops well short of the track's end at max scroll.
    let max_offset = app.lines.len().saturating_sub(app.viewport);
    let mut sb = ScrollbarState::new(max_offset.saturating_add(1))
        .position(app.offset)
        .viewport_content_length(app.viewport);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None),
        inner,
        &mut sb,
    );

    let keys = if app.mode == Mode::Search {
        format!(
            "search: {} ({}) · Enter jump · Esc cancel",
            app.query,
            app.matches.len()
        )
    } else if app.show_outline {
        format!(
            "↑/↓ j/k select & jump · Enter/→ jump · o/Tab toggle · / search · p open · e edit · q quit"
        )
    } else {
        format!(
            "↑/↓ j/k scroll · PgUp/PgDn space page · o/Tab outline · / search · p open · e edit · g/G top/bottom · q quit"
        )
    };
    frame.render_widget(
        Paragraph::new(keys).style(Style::default().fg(Color::White).bg(Color::DarkGray)),
        status_area,
    );
    // Right-aligned transient message (editor failures, etc.).
    if let Some(msg) = &app.status_msg {
        let msg_w = msg.chars().count() as u16;
        let hor = Layout::horizontal([Constraint::Min(0), Constraint::Length(msg_w)])
            .split(status_area);
        frame.render_widget(
            Paragraph::new(msg.clone()).style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            hor[1],
        );
    }
}

fn run(mut terminal: ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = loop {
        terminal.draw(|frame| ui(frame, app))?;
        if app.mode != Mode::Picker {
            let _ = place_images(app);
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                // Any key other than `e` clears the transient status message.
                if !matches!(k.code, KeyCode::Char('e')) {
                    app.status_msg = None;
                }
                match app.mode {
                Mode::Picker => match k.code {
                    KeyCode::Char('q') => break Ok(()),
                    KeyCode::Esc => {
                        if app.picker.cwd != app.picker.start {
                            app.picker.cwd.pop();
                            refresh_picker(app);
                        } else if app.picker.from_reader {
                            app.mode = Mode::Reader;
                            app.wrap_width = usize::MAX; // reflow to rebuild image rows
                        } else {
                            break Ok(());
                        }
                    }
                    KeyCode::Backspace => {
                        if app.picker.cwd != app.picker.start {
                            app.picker.cwd.pop();
                            refresh_picker(app);
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if !app.picker.entries.is_empty() {
                            app.picker.selected =
                                (app.picker.selected + 1).min(app.picker.entries.len() - 1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.picker.selected = app.picker.selected.saturating_sub(1)
                    }
                    KeyCode::PageDown => {
                        app.picker.selected = app
                            .picker
                            .selected
                            .saturating_add(10)
                            .min(app.picker.entries.len().saturating_sub(1))
                    }
                    KeyCode::PageUp => {
                        app.picker.selected = app.picker.selected.saturating_sub(10)
                    }
                    KeyCode::Char('g') => app.picker.selected = 0,
                    KeyCode::Char('G') => {
                        app.picker.selected = app.picker.entries.len().saturating_sub(1)
                    }
                    KeyCode::Char('h') => {
                        app.picker.show_hidden = !app.picker.show_hidden;
                        refresh_picker(app);
                    }
                    KeyCode::Enter => {
                        if let Some(entry) = app.picker.entries.get(app.picker.selected) {
                            let path = entry.path.clone();
                            if entry.is_dir {
                                app.picker.cwd = path;
                                refresh_picker(app);
                            } else {
                                load_file(app, &path);
                            }
                        }
                    }
                    _ => {}
                },
                Mode::Search => match k.code {
                    KeyCode::Esc => {
                        app.query.clear();
                        app.matches.clear();
                        app.mode = Mode::Reader;
                    }
                    KeyCode::Enter => {
                        if let Some(&first) = app.matches.first() {
                            app.offset = first;
                        }
                        app.query.clear();
                        app.matches.clear();
                        app.mode = Mode::Reader;
                    }
                    KeyCode::Backspace => {
                        app.query.pop();
                    }
                    KeyCode::Char(c) => {
                        app.query.push(c);
                    }
                    _ => {}
                },
                Mode::Reader => match k.code {
                    KeyCode::Char('q') => break Ok(()),
                    KeyCode::Esc => {
                        if app.show_outline {
                            app.show_outline = false;
                        } else {
                            break Ok(());
                        }
                    }
                    KeyCode::Char('/') => {
                        app.query.clear();
                        app.matches.clear();
                        app.mode = Mode::Search;
                    }
                    KeyCode::Char('e') => {
                        let Some(mut cmd) = find_editor() else {
                            app.status_msg = Some(
                                "no editor found: set $EDITOR (e.g. export EDITOR=nvim)".into(),
                            );
                            continue;
                        };
                        // Leave the TUI so the editor owns the terminal.
                        let _ = execute!(io::stdout(), DisableMouseCapture);
                        ratatui::restore();
                        let result = cmd.arg(&app.path).status();
                        // Back to the TUI: re-init, re-read, keep position.
                        terminal = ratatui::init();
                        let _ = execute!(io::stdout(), EnableMouseCapture);
                        app.status_msg = match result {
                            Ok(s) if s.success() => None,
                            Ok(s) => Some(format!("editor exited with {s}")),
                            Err(e) => Some(format!("editor failed to start: {e}")),
                        };
                        reload(app);
                    }
                    KeyCode::Char('p') => {
                        app.image_placed = vec![false; app.images.len()];
                        app.image_rows.clear();
                        let _ = io::stdout().write_all(&delete_all_images());
                        let _ = io::stdout().flush();
                        app.picker.from_reader = true;
                        app.picker.cwd =
                            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                        app.picker.start = app.picker.cwd.clone();
                        refresh_picker(app);
                        app.mode = Mode::Picker;
                    }
                    KeyCode::Char('o') | KeyCode::Tab => toggle_outline(app),
                    KeyCode::Char('j') | KeyCode::Down => {
                        if app.show_outline {
                            select_heading(app, app.selected + 1);
                        } else {
                            app.offset = app.offset.saturating_add(1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if app.show_outline {
                            select_heading(app, app.selected.saturating_sub(1));
                        } else {
                            app.offset = app.offset.saturating_sub(1);
                        }
                    }
                    KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                        if app.show_outline {
                            select_heading(app, app.selected);
                        }
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        app.offset = app.offset.saturating_add(app.viewport.saturating_sub(1).max(1))
                    }
                    KeyCode::PageUp => {
                        app.offset = app.offset.saturating_sub(app.viewport.saturating_sub(1).max(1))
                    }
                    KeyCode::Char('g') => app.offset = 0,
                    KeyCode::Char('G') => app.offset = usize::MAX,
                    KeyCode::Home => app.offset = 0,
                    _ => {}
                },
            }
            },
            Event::Mouse(m) => match app.mode {
                Mode::Picker => match m.kind {
                    MouseEventKind::ScrollDown => {
                        if !app.picker.entries.is_empty() {
                            app.picker.selected =
                                (app.picker.selected + 3).min(app.picker.entries.len() - 1);
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        app.picker.selected = app.picker.selected.saturating_sub(3)
                    }
                    _ => {}
                },
                _ => match m.kind {
                    MouseEventKind::ScrollDown => app.offset = app.offset.saturating_add(3),
                    MouseEventKind::ScrollUp => app.offset = app.offset.saturating_sub(3),
                    _ => {}
                },
            },
            Event::Resize(..) => {
                // Resizing clears the alternate screen, including cached image
                // data and placements. Drop stale placements and force the
                // next frame to re-transmit before placing again.
                let _ = io::stdout().write_all(&delete_all_images());
                app.image_placed.iter_mut().for_each(|p| *p = false);
            }
            _ => {}
        }
    };
    let _ = io::stdout().write_all(&delete_all_images());
    let _ = io::stdout().flush();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    result
}

fn main() -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let arg = std::env::args().nth(1);
    if let Some(ref path) = arg
        && std::fs::read_to_string(path).is_err()
    {
        eprintln!("error reading {path}");
        std::process::exit(1);
    }

    let mut app = if let Some(path) = arg {
        let text = std::fs::read_to_string(&path).unwrap();
        let base = Path::new(&path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let (content, headings, images, tables) = render(&text, &base);
        let filename = Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        let mut app = App::reader(content, headings, filename, images, tables);
        app.path = Path::new(&path).to_path_buf();
        app
    } else {
        let mut app = App::reader(Vec::new(), Vec::new(), "picker".to_string(), Vec::new(), Vec::new());
        app.picker = Picker::new(cwd.clone());
        app.mode = Mode::Picker;
        refresh_picker(&mut app);
        app
    };

    let terminal = ratatui::init();
    let res = run(terminal, &mut app);
    ratatui::restore();
    res
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn headings_and_paragraphs() {
        let (content, _, _, _) = render("# Title\n\nSome *em* **strong** text.", Path::new("."));
        let t = text_of(&content).join("\n");
        assert!(t.contains("Title"));
        assert!(t.contains("Some em strong text."));
    }

    fn flags(style: Style) -> Modifier {
        style.add_modifier
    }

    #[test]
    fn inline_styles() {
        let (content, _, _, _) = render("**bold** and *italic* and ~~strike~~", Path::new("."));
        let line = &content[0];
        let bold = line.spans.iter().find(|s| s.content.as_ref() == "bold").unwrap();
        assert!(flags(bold.style).contains(Modifier::BOLD));
        let italic = line.spans.iter().find(|s| s.content.as_ref() == "italic").unwrap();
        assert!(flags(italic.style).contains(Modifier::ITALIC));
        let strike = line.spans.iter().find(|s| s.content.as_ref() == "strike").unwrap();
        assert!(flags(strike.style).contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn lists_and_tasks() {
        let (content, _, _, _) = render("- one\n- two\n\n1. first\n2. second", Path::new("."));
        let t = text_of(&content).join("\n");
        assert!(t.contains("• one"));
        assert!(t.contains("• two"));
        assert!(t.contains("1. first"));
        assert!(t.contains("2. second"));
    }

    #[test]
    fn code_block_with_language() {
        let (content, _, _, _) = render("```rust\nfn main() {}\n```", Path::new("."));
        let t = text_of(&content);
        assert!(t[0].contains("rust"), "got: {t:?}");
        assert!(t[1].contains("fn main() {}"), "got: {t:?}");
    }

    #[test]
    fn code_block_without_language() {
        let (content, _, _, _) = render("```\nplain\n```", Path::new("."));
        let t = text_of(&content);
        // No language label: the first non-empty line is the code itself.
        assert!(t.iter().any(|l| l.contains("plain")), "got: {t:?}");
        assert!(!t.iter().any(|l| l.contains("rust")), "got: {t:?}");
    }

    #[test]
    fn links() {
        let (content, _, _, _) = render("[ratatui](https://ratatui.rs)", Path::new("."));
        assert!(text_of(&content).join("\n").contains("ratatui (https://ratatui.rs)"));
    }

    #[test]
    fn tables() {
        let (content, _, _, tables) = render("| a | b |\n|---|--:|\n| 1 | 2 |", Path::new("."));
        let mut app = App::reader(content, Vec::new(), "t".into(), Vec::new(), tables);
        reflow(&mut app, 40);
        let t = text_of(&app.lines).join("\n");
        assert!(t.contains("┌───┬───┐"), "got: {t:?}");
        assert!(t.contains("│ a │ b │"), "got: {t:?}");
        assert!(t.contains("├───┼───┤"), "got: {t:?}");
        assert!(t.contains("│ 1 │ 2 │"), "got: {t:?}");
        assert!(t.contains("└───┴───┘"), "got: {t:?}");
    }

    #[test]
    fn table_columns_align() {
        let (content, _, _, tables) = render(
            "| item | n |\n|------|--:|\n| a    | 2 |\n| b    | 100 |",
            Path::new("."),
        );
        let mut app = App::reader(content, Vec::new(), "t".into(), Vec::new(), tables);
        reflow(&mut app, 40);
        let t = text_of(&app.lines).join("\n");
        // The numeric column is right-aligned: short values get padded on the left.
        assert!(t.contains("│ a    │   2 │"), "got: {t:?}");
        assert!(t.contains("│ b    │ 100 │"), "got: {t:?}");
    }

    #[test]
    fn wide_table_wraps_cells() {
        let long = "x".repeat(120);
        let md = format!("| key | value |\n|---|---|\n| a | {long} |");
        let (content, _, _, tables) = render(&md, Path::new("."));
        let mut app = App::reader(content, Vec::new(), "t".into(), Vec::new(), tables);
        reflow(&mut app, 30);
        let t = text_of(&app.lines);
        for line in &t {
            assert!(
                line.chars().count() <= 30,
                "table line exceeds width: {line:?}"
            );
        }
        // Structure is intact: top border, bottom border, and the wrapped body
        // row is separated from the header by a grid separator.
        assert!(t[0].starts_with('┌') && t[0].ends_with('┐'), "got: {t:?}");
        assert!(t.last().unwrap().starts_with('└'), "got: {t:?}");
        assert!(
            t.iter().filter(|l| l.starts_with('├')).count() >= 1,
            "wrapped rows need grid separators, got: {t:?}"
        );
        // The whole long cell is visible, just wrapped across several lines.
        let all: String = t.join("");
        assert!(
            all.chars().filter(|&c| c == 'x').count() >= 120,
            "long cell should be fully visible, got: {t:?}"
        );
    }

    #[test]
    fn blockquote() {
        let (content, _, _, _) = render("> hello", Path::new("."));
        assert!(text_of(&content).join("\n").contains("▎ hello"));
    }

    #[test]
    fn images_emit_placeholder() {
        let (content, _, images, _) = render("![alt text](img.png)", Path::new("."));
        let t = text_of(&content);
        assert!(t.iter().any(|l| l.contains("alt text")), "got: {t:?}");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].content_line, 0);
        assert!(images[0].dims.is_none()); // file doesn't exist
    }

    #[test]
    fn images_reserve_space_in_reflow() {
        let dir = std::env::temp_dir();
        let path = dir.join("md_test_img.png");
        let img = image::RgbaImage::from_pixel(200, 100, image::Rgba([255, 0, 0, 255]));
        img.save(&path).unwrap();
        let md = format!("![x]({})", path.display());
        let (content, _, images, _) = render(&md, Path::new("."));
        let mut app = App::reader(content, Vec::new(), "t".into(), images, Vec::new());
        reflow(&mut app, 40);
        assert!(app.images[0].cw > 0, "image should get a cell width");
        assert!(app.images[0].ch >= 3, "image should get a cell height");
        assert_eq!(app.image_rows.len(), 1);
        assert!(
            app.lines.len() > 1,
            "image should reserve blank lines, got {}",
            app.lines.len()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn encode_png_downscales_large_images() {
        let dir = std::env::temp_dir();
        let path = dir.join("md_test_big.png");
        let img = image::RgbaImage::from_pixel(4000, 2000, image::Rgba([255, 0, 0, 255]));
        img.save(&path).unwrap();
        let (png, w, h) = encode_png(&path, 640).unwrap();
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (w, h));
        assert!(decoded.width() <= 640, "longest side should be capped");
        assert!(decoded.height() <= 640);
        assert!(decoded.width() < 4000, "large image should be shrunk");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn outline_tracks_headings() {
        let (_, headings, _, _) = render("# Title\n\n## Sub\n\n### Deep", Path::new("."));
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0].title, "Title");
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[0].content_line, 0);
        assert_eq!(headings[1].title, "Sub");
        assert_eq!(headings[1].level, 2);
        assert_eq!(headings[2].title, "Deep");
        assert_eq!(headings[2].level, 3);
    }

    #[test]
    fn in_path_finds_shell_and_rejects_garbage() {
        assert!(
            in_path("sh") || in_path("bash") || in_path("zsh"),
            "no common shell found on PATH"
        );
        assert!(!in_path("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn wrap_breaks_words() {
        let (content, _, _, _) = render("one two three four", Path::new("."));
        let wrapped = wrap_line(&content[0], 7);
        let t = text_of(&wrapped);
        assert_eq!(t, vec!["one two", "three", "four"], "got: {t:?}");
    }

    #[test]
    fn wrap_hard_breaks_long_words() {
        let (content, _, _, _) = render("abcdefghij", Path::new("."));
        let wrapped = wrap_line(&content[0], 4);
        let t = text_of(&wrapped);
        assert_eq!(t, vec!["abcd", "efgh", "ij"], "got: {t:?}");
    }

    #[test]
    fn reflow_updates_heading_lines() {
        let (content, headings, _, _) = render("# Title\n\nlong paragraph text here", Path::new("."));
        let mut app = App::reader(content, headings, "test".into(), Vec::new(), Vec::new());
        reflow(&mut app, 8);
        assert_eq!(app.headings[0].line, 0);
        assert!(app.lines.len() > app.content.len());
    }

    #[test]
    fn kitty_place_and_delete_escapes() {
        let p = String::from_utf8(place_image(3, 10, 20, 30, 5, None)).unwrap();
        assert!(p.contains("\x1b[10;20H"));
        assert!(p.contains("\x1b_Ga=p,i=3,p=3,c=30,r=5,q=2,C=1;\x1b\\"));
        // A source rectangle clips the image when partially visible.
        let clipped = String::from_utf8(place_image(3, 10, 20, 30, 3, Some((0, 40, 100, 60)))).unwrap();
        assert!(clipped.contains("c=30,r=3,q=2,C=1,x=0,y=40,w=100,h=20"), "got: {clipped}");
        let d = String::from_utf8(delete_image(3)).unwrap();
        assert!(d.contains("\x1b_Ga=d,d=i,i=3,q=2;\x1b\\"));
        let da = String::from_utf8(delete_all_images()).unwrap();
        assert!(da.contains("d=a"));
    }

    #[test]
    fn reload_keeps_position_and_content() {
        let dir = std::env::temp_dir();
        let path = dir.join("md_reload_test.md");
        std::fs::write(&path, "# A\n\nsome text").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let (content, headings, images, tables) = render(&text, &dir);
        let mut app = App::reader(content, headings, "t".into(), images, tables);
        app.path = path.clone();
        app.offset = 2;
        reflow(&mut app, 40);
        assert_eq!(app.offset, 2);

        // Simulate editing: add lines at the top, then reload.
        std::fs::write(&path, "# A\n\nnew line added\n\nsome text").unwrap();
        reload(&mut app);
        let texts = text_of(&app.content);
        assert!(
            texts.iter().any(|t| t.contains("new line added")),
            "reload should pick up edits, got: {texts:?}"
        );
        reflow(&mut app, 40);
        // Scroll position is kept (and still valid after the content grew).
        assert_eq!(app.offset, 2);
        assert!(app.offset <= app.lines.len());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kitty_transmit_chunks() {
        // 10k bytes -> ~13.3k base64 chars -> 4 chunks of <=4096.
        let data = vec![0u8; 10_000];
        let esc_bytes = transmit_image(7, &data);
        let esc = String::from_utf8_lossy(&esc_bytes);
        assert!(esc.starts_with("\x1b_Ga=t,f=100,i=7,q=2,m=1;"));
        assert!(esc.contains("\x1b_Gm=1;"));
        assert!(esc.contains("m=0;"));
        assert!(esc.ends_with("\x1b\\"));
        // Every chunk is a complete escape terminated by ST.
        let chunks: Vec<&str> = esc.split("\x1b\\").collect();
        assert!(chunks.len() >= 4);
    }
}
