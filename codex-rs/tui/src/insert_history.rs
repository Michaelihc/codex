//! Inserts finalized history rows into terminal scrollback.
//!
//! Codex uses the terminal scrollback itself for finalized chat history, so inserting a history
//! cell is an escape-sequence operation rather than a normal ratatui render.

use std::fmt;
use std::io;
use std::io::Write;

use crate::render::line_utils::line_to_static;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::decorate_spans;
use crate::terminal_hyperlinks::plain_hyperlink_lines;
use crate::terminal_hyperlinks::remap_wrapped_line;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;
use crate::wrapping::line_contains_url_like;
use crate::wrapping::line_has_mixed_url_and_non_url_tokens;
use crossterm::Command;
use crossterm::cursor::MoveDown;
use crossterm::cursor::MoveTo;
use crossterm::cursor::MoveToColumn;
use crossterm::cursor::RestorePosition;
use crossterm::cursor::SavePosition;
use crossterm::queue;
use crossterm::style::Color as CColor;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use ratatui::layout::Size;
use ratatui::prelude::Backend;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryLineWrapPolicy {
    PreWrap,
    Terminal,
}

/// Selects the terminal escape strategy used when writing history above the viewport.
///
/// Raw lines intentionally remain unbroken so terminal selection copies their source faithfully.
/// Zellij does not constrain soft-wrapped continuation rows to Codex's scroll region, so its raw
/// path appends history through the terminal and reserves blank rows for the next viewport draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertHistoryMode {
    Standard,
    ZellijRaw,
}

/// Insert `lines` above the viewport using the terminal's backend writer
/// (avoids direct stdout references).
pub fn insert_history_lines<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
    lines: Vec<Line>,
) -> io::Result<()>
where
    B: Backend + Write,
{
    insert_history_lines_with_wrap_policy(terminal, lines, HistoryLineWrapPolicy::PreWrap)
}

pub fn insert_history_lines_with_wrap_policy<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
    lines: Vec<Line>,
    wrap_policy: HistoryLineWrapPolicy,
) -> io::Result<()>
where
    B: Backend + Write,
{
    insert_history_lines_with_mode_and_wrap_policy(
        terminal,
        lines,
        InsertHistoryMode::Standard,
        wrap_policy,
    )
}

pub(crate) fn insert_history_lines_with_mode_and_wrap_policy<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
    lines: Vec<Line>,
    mode: InsertHistoryMode,
    wrap_policy: HistoryLineWrapPolicy,
) -> io::Result<()>
where
    B: Backend + Write,
{
    insert_history_hyperlink_lines_with_mode_and_wrap_policy(
        terminal,
        plain_hyperlink_lines(lines.iter().map(line_to_static).collect()),
        mode,
        wrap_policy,
    )
}

pub(crate) fn insert_history_hyperlink_lines_with_mode_and_wrap_policy<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
    lines: Vec<HyperlinkLine>,
    mode: InsertHistoryMode,
    wrap_policy: HistoryLineWrapPolicy,
) -> io::Result<()>
where
    B: Backend + Write,
{
    let screen_size = terminal.backend().size().unwrap_or(Size::new(0, 0));

    let mut area = terminal.viewport_area;
    let mut should_update_area = false;
    let last_cursor_pos = terminal.last_known_cursor_pos;

    // Pre-wrap lines for terminal scrollback. Three paths:
    //
    // - URL-only-ish lines are kept intact (no hard newlines inserted) so that
    //   terminal emulators can match them as clickable links. The
    //   terminal will character-wrap these lines at the viewport
    //   boundary.
    // - Mixed lines (URL + non-URL prose) are adaptively wrapped so
    //   non-URL text still wraps naturally while URL tokens remain
    //   unsplit.
    // - Non-URL lines also flow through adaptive wrapping; behavior is
    //   equivalent to standard wrapping when no URL is present.
    let wrap_width = area.width.max(1) as usize;
    let mut wrapped = Vec::new();
    let mut wrapped_rows = 0usize;

    for line in &lines {
        let line_wrapped = match wrap_policy {
            HistoryLineWrapPolicy::Terminal => vec![line.clone()],
            HistoryLineWrapPolicy::PreWrap
                if line_contains_url_like(&line.line)
                    && !line_has_mixed_url_and_non_url_tokens(&line.line) =>
            {
                vec![line.clone()]
            }
            HistoryLineWrapPolicy::PreWrap => remap_wrapped_line(
                line,
                adaptive_wrap_line(
                    &line.line,
                    RtOptions::new(wrap_width)
                        .subsequent_indent(leading_whitespace_prefix(&line.line)),
                )
                .into_iter()
                .map(|line| line_to_static(&line))
                .collect(),
            ),
        };
        wrapped_rows += line_wrapped
            .iter()
            .map(|wrapped_line| wrapped_line.width().max(1).div_ceil(wrap_width))
            .sum::<usize>();
        wrapped.extend(line_wrapped);
    }
    let wrapped_lines = wrapped_rows as u16;
    match mode {
        InsertHistoryMode::ZellijRaw => {
            // The existing viewport is immediately replaced in the same draw pass. Clear it
            // before terminal scrolling can move composer contents into scrollback.
            terminal.clear_after_position(area.as_position())?;
            let writer = terminal.backend_mut();
            queue!(writer, MoveTo(/*x*/ 0, area.top()))?;
            for (index, line) in wrapped.iter().enumerate() {
                if index > 0 {
                    queue!(writer, Print("\r\n"))?;
                }
                write_history_line(writer, line, wrap_width)?;
            }

            // Writing raw source text through the terminal preserves its soft-wrap metadata.
            // Advance through empty rows for the viewport so history ends immediately above the
            // composer even when a replay batch is taller than the visible history region.
            for _ in 0..area.height {
                queue!(writer, Print("\r\n"), Clear(ClearType::UntilNewLine))?;
            }
            queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;

            let viewport_top = area
                .top()
                .saturating_add(wrapped_lines)
                .min(screen_size.height.saturating_sub(area.height));
            if area.y != viewport_top {
                area.y = viewport_top;
                should_update_area = true;
            }
        }
        InsertHistoryMode::Standard => {
            let writer = terminal.backend_mut();
            let cursor_top = if area.bottom() < screen_size.height {
                // If the viewport is not at the bottom of the screen, scroll it down to make room.
                // Don't scroll it past the bottom of the screen.
                let scroll_amount = wrapped_lines.min(screen_size.height - area.bottom());

                let top_1based = area.top() + 1;
                queue!(writer, SetScrollRegion(top_1based..screen_size.height))?;
                queue!(writer, MoveTo(/*x*/ 0, area.top()))?;
                for _ in 0..scroll_amount {
                    queue!(writer, Print("\x1bM"))?;
                }
                queue!(writer, ResetScrollRegion)?;

                let cursor_top = area.top().saturating_sub(1);
                area.y += scroll_amount;
                should_update_area = true;
                cursor_top
            } else {
                area.top().saturating_sub(1)
            };

            // Limit the scroll region to the lines from the top of the screen to the
            // top of the viewport. With this in place, when we add lines inside this
            // area, only the lines in this area will be scrolled. We place the cursor
            // at the end of the scroll region, and add lines starting there.
            //
            // ┌─Screen───────────────────────┐
            // │┌╌Scroll region╌╌╌╌╌╌╌╌╌╌╌╌╌╌┐│
            // │┆                            ┆│
            // │┆                            ┆│
            // │┆                            ┆│
            // │█╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┘│
            // │╭─Viewport───────────────────╮│
            // ││                            ││
            // │╰────────────────────────────╯│
            // └──────────────────────────────┘
            queue!(writer, SetScrollRegion(1..area.top()))?;

            // NB: we are using MoveTo instead of set_cursor_position here to avoid messing with the
            // terminal's last_known_cursor_position, which hopefully will still be accurate after we
            // fetch/restore the cursor position. insert_history_lines should be cursor-position-neutral :)
            queue!(writer, MoveTo(/*x*/ 0, cursor_top))?;

            for line in &wrapped {
                queue!(writer, Print("\r\n"))?;
                write_history_line(writer, line, wrap_width)?;
            }

            queue!(writer, ResetScrollRegion)?;
            queue!(writer, MoveTo(last_cursor_pos.x, last_cursor_pos.y))?;
        }
    }

    if should_update_area {
        terminal.set_viewport_area(area);
    }
    if wrapped_lines > 0 {
        terminal.note_history_rows_inserted(wrapped_lines);
    }

    Ok(())
}

pub(crate) fn leading_whitespace_prefix(line: &Line<'_>) -> Line<'static> {
    let mut spans = Vec::new();
    for span in &line.spans {
        let prefix_end = span
            .content
            .char_indices()
            .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
            .unwrap_or(span.content.len());
        if prefix_end > 0 {
            spans.push(Span::styled(
                span.content[..prefix_end].to_string(),
                span.style,
            ));
        }
        if prefix_end < span.content.len() {
            break;
        }
    }
    Line::from(spans).style(line.style)
}

/// Render a single wrapped history line: clear continuation rows for wide lines,
/// set foreground/background colors, and write styled spans. Caller is responsible
/// for cursor positioning and any leading `\r\n`.
fn write_history_line<W: Write>(
    writer: &mut W,
    line: &HyperlinkLine,
    wrap_width: usize,
) -> io::Result<()> {
    let physical_rows = line.width().max(1).div_ceil(wrap_width) as u16;
    if physical_rows > 1 {
        queue!(writer, SavePosition)?;
        for _ in 1..physical_rows {
            queue!(writer, MoveDown(1), MoveToColumn(0))?;
            queue!(writer, Clear(ClearType::UntilNewLine))?;
        }
        queue!(writer, RestorePosition)?;
    }
    queue!(
        writer,
        SetColors(Colors::new(
            line.line
                .style
                .fg
                .map(std::convert::Into::into)
                .unwrap_or(CColor::Reset),
            line.line
                .style
                .bg
                .map(std::convert::Into::into)
                .unwrap_or(CColor::Reset)
        ))
    )?;
    queue!(writer, Clear(ClearType::UntilNewLine))?;
    // Merge line-level style into each span so that ANSI colors reflect
    // line styles (e.g., blockquotes with green fg).
    let merged_spans: Vec<Span> = line
        .line
        .spans
        .iter()
        .map(|s| Span {
            style: s.style.patch(line.line.style),
            content: s.content.clone(),
        })
        .collect();
    let merged_line = HyperlinkLine {
        line: Line::from(merged_spans),
        hyperlinks: line.hyperlinks.clone(),
    };
    let decorated = decorate_spans(&merged_line);
    write_spans(writer, decorated.iter())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetScrollRegion(pub std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        panic!("tried to execute SetScrollRegion command using WinAPI, use ANSI instead");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        // TODO(nornagon): is this supported on Windows?
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetScrollRegion;

impl Command for ResetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        panic!("tried to execute ResetScrollRegion command using WinAPI, use ANSI instead");
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        // TODO(nornagon): is this supported on Windows?
        true
    }
}

struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    fn queue<W>(self, mut w: W) -> io::Result<()>
    where
        W: io::Write,
    {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}

fn write_spans<'a, I>(mut writer: &mut impl Write, content: I) -> io::Result<()>
where
    I: IntoIterator<Item = &'a Span<'a>>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut last_modifier = Modifier::empty();
    for span in content {
        let mut modifier = Modifier::empty();
        modifier.insert(span.style.add_modifier);
        modifier.remove(span.style.sub_modifier);
        if modifier != last_modifier {
            let diff = ModifierDiff {
                from: last_modifier,
                to: modifier,
            };
            diff.queue(&mut writer)?;
            last_modifier = modifier;
        }
        let next_fg = span.style.fg.unwrap_or(Color::Reset);
        let next_bg = span.style.bg.unwrap_or(Color::Reset);
        if next_fg != fg || next_bg != bg {
            queue!(
                writer,
                SetColors(Colors::new(next_fg.into(), next_bg.into()))
            )?;
            fg = next_fg;
            bg = next_bg;
        }

        queue!(writer, Print(span.content.clone()))?;
    }

    queue!(
        writer,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_render::render_markdown_text;
    use crate::test_backend::VT100Backend;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    #[test]
    fn writes_bold_then_regular_spans() {
        use ratatui::style::Stylize;

        let spans = ["A".bold(), "B".into()];

        let mut actual: Vec<u8> = Vec::new();
        write_spans(&mut actual, spans.iter()).unwrap();

        let mut expected: Vec<u8> = Vec::new();
        queue!(
            expected,
            SetAttribute(crossterm::style::Attribute::Bold),
            Print("A"),
            SetAttribute(crossterm::style::Attribute::NormalIntensity),
            Print("B"),
            SetForegroundColor(CColor::Reset),
            SetBackgroundColor(CColor::Reset),
            SetAttribute(crossterm::style::Attribute::Reset),
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(actual).unwrap(),
            String::from_utf8(expected).unwrap()
        );
    }

    #[test]
    fn writes_semantic_web_link_without_changing_visible_text() {
        let destination = "https://example.com/long/path";
        let line = crate::terminal_hyperlinks::annotate_web_urls_in_line(Line::from(destination));
        let mut actual = Vec::new();

        write_history_line(&mut actual, &line, /*wrap_width*/ 80).expect("write history line");

        let output = String::from_utf8(actual).expect("UTF-8 terminal output");
        assert!(output.contains("\x1b]8;;https://example.com/long/path\x07"));
        assert_eq!(line.line.spans[0].content, destination);
    }

    #[test]
    fn vt100_blockquote_line_emits_green_fg() {
        // Set up a small off-screen terminal
        let width: u16 = 40;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        // Place viewport on the last line so history inserts scroll upward
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        // Build a blockquote-like line: apply line-level green style and prefix "> "
        let mut line: Line<'static> = Line::from(vec!["> ".into(), "Hello world".into()]);
        line = line.style(Color::Green);
        insert_history_lines(&mut term, vec![line])
            .expect("Failed to insert history lines in test");

        let mut saw_colored = false;
        'outer: for row in 0..height {
            for col in 0..width {
                if let Some(cell) = term.backend().vt100().screen().cell(row, col)
                    && cell.has_contents()
                    && cell.fgcolor() != vt100::Color::Default
                {
                    saw_colored = true;
                    break 'outer;
                }
            }
        }
        assert!(
            saw_colored,
            "expected at least one colored cell in vt100 output"
        );
    }

    #[test]
    fn vt100_blockquote_wrap_preserves_color_on_all_wrapped_lines() {
        // Force wrapping by using a narrow viewport width and a long blockquote line.
        let width: u16 = 20;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        // Viewport is the last line so history goes directly above it.
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        // Create a long blockquote with a distinct prefix and enough text to wrap.
        let mut line: Line<'static> = Line::from(vec![
            "> ".into(),
            "This is a long quoted line that should wrap".into(),
        ]);
        line = line.style(Color::Green);

        insert_history_lines(&mut term, vec![line])
            .expect("Failed to insert history lines in test");

        // Parse and inspect the final screen buffer.
        let screen = term.backend().vt100().screen();

        // Collect rows that are non-empty; these should correspond to our wrapped lines.
        let mut non_empty_rows: Vec<u16> = Vec::new();
        for row in 0..height {
            let mut any = false;
            for col in 0..width {
                if let Some(cell) = screen.cell(row, col)
                    && cell.has_contents()
                    && cell.contents() != "\0"
                    && cell.contents() != " "
                {
                    any = true;
                    break;
                }
            }
            if any {
                non_empty_rows.push(row);
            }
        }

        // Expect at least two rows due to wrapping.
        assert!(
            non_empty_rows.len() >= 2,
            "expected wrapped output to span >=2 rows, got {non_empty_rows:?}",
        );

        // For each non-empty row, ensure all non-space cells are using a non-default fg color.
        for row in non_empty_rows {
            for col in 0..width {
                if let Some(cell) = screen.cell(row, col) {
                    let contents = cell.contents();
                    if !contents.is_empty() && contents != " " {
                        assert!(
                            cell.fgcolor() != vt100::Color::Default,
                            "expected non-default fg on row {row} col {col}, got {:?}",
                            cell.fgcolor()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn vt100_colored_prefix_then_plain_text_resets_color() {
        let width: u16 = 40;
        let height: u16 = 6;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        // First span colored, rest plain.
        let line: Line<'static> = Line::from(vec![
            Span::styled("1. ", ratatui::style::Style::default().fg(Color::LightBlue)),
            Span::raw("Hello world"),
        ]);

        insert_history_lines(&mut term, vec![line])
            .expect("Failed to insert history lines in test");

        let screen = term.backend().vt100().screen();

        // Find the first non-empty row; verify first three cells are colored, following cells default.
        'rows: for row in 0..height {
            let mut has_text = false;
            for col in 0..width {
                if let Some(cell) = screen.cell(row, col)
                    && cell.has_contents()
                    && cell.contents() != " "
                {
                    has_text = true;
                    break;
                }
            }
            if !has_text {
                continue;
            }

            // Expect "1. Hello world" starting at col 0.
            for col in 0..3 {
                let cell = screen.cell(row, col).unwrap();
                assert!(
                    cell.fgcolor() != vt100::Color::Default,
                    "expected colored prefix at col {col}, got {:?}",
                    cell.fgcolor()
                );
            }
            for col in 3..(3 + "Hello world".len() as u16) {
                let cell = screen.cell(row, col).unwrap();
                assert_eq!(
                    cell.fgcolor(),
                    vt100::Color::Default,
                    "expected default color for plain text at col {col}, got {:?}",
                    cell.fgcolor()
                );
            }
            break 'rows;
        }
    }

    #[test]
    fn vt100_deep_nested_mixed_list_third_level_marker_is_colored() {
        // Markdown with five levels (ordered → unordered → ordered → unordered → unordered).
        let md = "1. First\n   - Second level\n     1. Third level (ordered)\n        - Fourth level (bullet)\n          - Fifth level to test indent consistency\n";
        let text = render_markdown_text(md);
        let lines: Vec<Line<'static>> = text.lines.clone();

        let width: u16 = 60;
        let height: u16 = 12;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = ratatui::layout::Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        insert_history_lines(&mut term, lines).expect("Failed to insert history lines in test");

        let screen = term.backend().vt100().screen();

        // Reconstruct screen rows as strings to locate the 3rd level line.
        let rows: Vec<String> = screen.rows(0, width).collect();

        let needle = "1. Third level (ordered)";
        let row_idx = rows
            .iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| {
                panic!("expected to find row containing {needle:?}, have rows: {rows:?}")
            });
        let col_start = rows[row_idx].find(needle).unwrap() as u16; // column where '1' starts

        // Verify that the numeric marker ("1.") at the third level is colored
        // (non-default fg) and the content after the following space resets to default.
        for c in [col_start, col_start + 1] {
            let cell = screen.cell(row_idx as u16, c).unwrap();
            assert!(
                cell.fgcolor() != vt100::Color::Default,
                "expected colored 3rd-level marker at row {row_idx} col {c}, got {:?}",
                cell.fgcolor()
            );
        }
        let content_col = col_start + 3; // skip '1', '.', and the space
        if let Some(cell) = screen.cell(row_idx as u16, content_col) {
            assert_eq!(
                cell.fgcolor(),
                vt100::Color::Default,
                "expected default color for 3rd-level content at row {row_idx} col {content_col}, got {:?}",
                cell.fgcolor()
            );
        }
    }

    #[test]
    fn vt100_prefixed_url_keeps_prefix_and_url_on_same_row() {
        let width: u16 = 48;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let url = "http://a-long-url.com/this/that/blablablab/new.aspx/many_people_like_how";
        let line: Line<'static> = Line::from(vec!["  │ ".into(), url.into()]);

        insert_history_lines(&mut term, vec![line]).expect("insert history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();

        assert!(
            rows.iter().any(|r| r.contains("│ http://a-long-url.com")),
            "expected prefix and URL on same row, rows: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.trim_end() == "│"),
            "unexpected orphan prefix row, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_prefixed_url_like_without_scheme_keeps_prefix_and_token_on_same_row() {
        let width: u16 = 48;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let url_like =
            "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890";
        let line: Line<'static> = Line::from(vec!["  │ ".into(), url_like.into()]);

        insert_history_lines(&mut term, vec![line]).expect("insert history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();

        assert!(
            rows.iter()
                .any(|r| r.contains("│ example.test/api/v1/projects")),
            "expected prefix and URL-like token on same row, rows: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.trim_end() == "│"),
            "unexpected orphan prefix row, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_prefixed_mixed_url_line_wraps_suffix_words_together() {
        let width: u16 = 24;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let url = "https://example.test/path/abcdef12345";
        let line: Line<'static> = Line::from(vec![
            "  │ ".into(),
            "see ".into(),
            url.into(),
            " tail words".into(),
        ]);

        insert_history_lines(&mut term, vec![line]).expect("insert mixed history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(
            rows.iter().any(|r| r.contains("│ see")),
            "expected prefixed prose before URL, rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("tail words")),
            "expected suffix words to wrap as a phrase, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_prefixed_mixed_url_line_preserves_prefix_on_wrapped_rows() {
        let width: u16 = 24;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(
            /*x*/ 0,
            /*y*/ height - 1,
            /*width*/ width,
            /*height*/ 1,
        );
        term.set_viewport_area(viewport);

        let line: Line<'static> = Line::from(vec![
            "  ".into(),
            "see https://example.com and enough trailing prose to force another wrapped row".into(),
        ]);

        insert_history_lines(&mut term, vec![line]).expect("insert mixed history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        let continuation_row = rows
            .iter()
            .find(|row| row.contains("prose to force another"))
            .unwrap_or_else(|| panic!("expected continuation row in screen rows: {rows:?}"));

        assert!(
            continuation_row.starts_with("  "),
            "expected wrapped continuation row to keep the original prefix, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_prefixed_non_url_line_preserves_prefix_on_wrapped_rows() {
        let width: u16 = 32;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(
            /*x*/ 0,
            /*y*/ height - 1,
            /*width*/ width,
            /*height*/ 1,
        );
        term.set_viewport_area(viewport);

        let line = Line::from(
            "      dog while this deliberately long string tests code block scrolling versus soft wrapping",
        );

        insert_history_lines(&mut term, vec![line]).expect("insert prefixed history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        let continuation_row = rows
            .iter()
            .find(|row| row.contains("tests code block scrolling"))
            .unwrap_or_else(|| panic!("expected continuation row in screen rows: {rows:?}"));

        assert!(
            continuation_row.starts_with("      "),
            "expected wrapped continuation row to keep the original prefix, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_terminal_wrap_policy_does_not_pre_wrap_long_paragraph() {
        let width: u16 = 20;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let line = Line::from("alpha beta gamma delta epsilon zeta");

        insert_history_lines_with_wrap_policy(
            &mut term,
            vec![line],
            HistoryLineWrapPolicy::Terminal,
        )
        .expect("insert raw history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        assert!(
            rows.iter()
                .any(|row| row.trim_end() == "alpha beta gamma del"),
            "expected terminal soft-wrap instead of Codex word pre-wrap, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_zellij_raw_insert_keeps_soft_wrapped_tail_above_viewport() {
        let width: u16 = 20;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(
            /*x*/ 0,
            /*y*/ height - 2,
            /*width*/ width,
            /*height*/ 2,
        );
        term.set_viewport_area(viewport);

        let line = Line::from("raw-start-aaaaaaaaaaaaaaaaaaaaaaaa-tail-must-remain");
        insert_history_lines_with_mode_and_wrap_policy(
            &mut term,
            vec![line],
            InsertHistoryMode::ZellijRaw,
            HistoryLineWrapPolicy::Terminal,
        )
        .expect("insert Zellij raw history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        insta::assert_snapshot!("zellij_raw_terminal_wrap_above_viewport", rows.join("\n"));
        let history_rows = rows[..usize::from(term.viewport_area.y)]
            .iter()
            .map(|row| row.trim_end())
            .collect::<String>();
        let viewport_rows = rows[usize::from(term.viewport_area.y)..].join("\n");
        assert!(
            history_rows.contains("tail-must-remain"),
            "expected wrapped raw tail above the viewport, rows: {rows:?}"
        );
        assert!(
            !viewport_rows.contains("tail-must-remain"),
            "raw tail must not be written through the viewport, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_zellij_raw_replay_keeps_overflowing_soft_wrapped_tail_above_viewport() {
        let width: u16 = 20;
        let height: u16 = 8;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        term.set_viewport_area(Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ width, /*height*/ 2,
        ));

        let line = Line::from(format!("raw-start-{}tail-must-remain", "a".repeat(130)));
        insert_history_lines_with_mode_and_wrap_policy(
            &mut term,
            vec![line],
            InsertHistoryMode::ZellijRaw,
            HistoryLineWrapPolicy::Terminal,
        )
        .expect("replay Zellij raw history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        insta::assert_snapshot!(
            "zellij_raw_terminal_wrap_overflow_above_viewport",
            rows.join("\n")
        );
        let history_rows = rows[..usize::from(term.viewport_area.y)]
            .iter()
            .map(|row| row.trim_end())
            .collect::<String>();
        let viewport_rows = rows[usize::from(term.viewport_area.y)..].join("\n");
        assert!(
            history_rows.contains("tail-must-remain"),
            "expected overflowing raw tail above the viewport, rows: {rows:?}"
        );
        assert!(
            !viewport_rows.contains("tail-must-remain"),
            "overflowing raw tail must not be written through the viewport, rows: {rows:?}"
        );
    }

    #[test]
    fn vt100_unwrapped_url_like_clears_continuation_rows() {
        let width: u16 = 20;
        let height: u16 = 10;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let filler_line: Line<'static> = Line::from(vec![
            "  │ ".into(),
            "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into(),
        ]);
        insert_history_lines(&mut term, vec![filler_line]).expect("insert filler history");

        let url_like = "example.test/api/v1/short";
        let url_line: Line<'static> = Line::from(vec!["  │ ".into(), url_like.into()]);
        insert_history_lines(&mut term, vec![url_line]).expect("insert url-like history");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        let first_row = rows
            .iter()
            .position(|row| row.contains("│ example.test/api"))
            .unwrap_or_else(|| panic!("expected url-like first row in screen rows: {rows:?}"));
        assert!(
            first_row + 1 < rows.len(),
            "expected a continuation row for wrapped URL-like line, rows: {rows:?}"
        );
        let continuation_row = rows[first_row + 1].trim_end();

        assert!(
            continuation_row.contains("/v1/short") || continuation_row.contains("short"),
            "expected continuation row to contain wrapped URL-like tail, got: {continuation_row:?}"
        );
        assert!(
            !continuation_row.contains('X'),
            "expected continuation row to be cleared before writing wrapped URL-like content, got: {continuation_row:?}"
        );
    }

    #[test]
    fn vt100_long_unwrapped_url_does_not_insert_extra_blank_gap_before_content() {
        let width: u16 = 56;
        let height: u16 = 24;
        let backend = VT100Backend::new(width, height);
        let mut term = crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        let viewport = Rect::new(0, height - 1, width, 1);
        term.set_viewport_area(viewport);

        let prompt = "Write a long URL as output for testing";
        insert_history_lines(&mut term, vec![Line::from(prompt)]).expect("insert prompt line");

        let long_url = format!(
            "https://example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/{}",
            "very-long-segment-".repeat(16),
        );
        let url_line: Line<'static> = Line::from(vec!["• ".into(), long_url.into()]);
        insert_history_lines(&mut term, vec![url_line]).expect("insert long url line");

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        let prompt_row = rows
            .iter()
            .position(|row| row.contains("Write a long URL as output for testing"))
            .unwrap_or_else(|| panic!("expected prompt row in screen rows: {rows:?}"));
        let url_row = rows
            .iter()
            .position(|row| row.contains("• https://example.test/api"))
            .unwrap_or_else(|| panic!("expected URL first row in screen rows: {rows:?}"));

        assert!(
            url_row <= prompt_row + 2,
            "expected URL content to appear immediately after prompt (allowing at most one spacer row), got prompt_row={prompt_row}, url_row={url_row}, rows={rows:?}",
        );
    }

    // --- Windows / ConPTY scroll-region regression coverage ---------------------------------
    //
    // `VT100Backend` faithfully honors DECSTBM scroll-region margins, which is exactly why the
    // `Standard` insertion path (which depends on them) looks correct in every other test. The
    // backend below models a host that IGNORES those margins (legacy conhost / some ConPTY
    // builds): it strips `CSI <params> r` from the byte stream before delegating to a real
    // `VT100Backend`, so reverse-index and line feeds act on the whole screen — the conditions
    // under which finalized history gets overwritten in the field (openai/codex#15380).

    #[derive(Clone, Copy, PartialEq)]
    enum EscState {
        Normal,
        Esc,
        Csi,
    }

    struct NoScrollRegionBackend {
        inner: VT100Backend,
        state: EscState,
        csi: Vec<u8>,
        raw: Vec<u8>,
    }

    impl NoScrollRegionBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: VT100Backend::new(width, height),
                state: EscState::Normal,
                csi: Vec::new(),
                raw: Vec::new(),
            }
        }

        fn vt100(&self) -> &vt100::Parser {
            self.inner.vt100()
        }

        /// All bytes written by the caller, before filtering — used to assert which escape
        /// primitives a path actually emits.
        fn raw(&self) -> &[u8] {
            &self.raw
        }

        fn filter(&mut self, buf: &[u8], out: &mut Vec<u8>) {
            for &b in buf {
                match self.state {
                    EscState::Normal => {
                        if b == 0x1b {
                            self.state = EscState::Esc;
                        } else {
                            out.push(b);
                        }
                    }
                    EscState::Esc => {
                        if b == b'[' {
                            self.state = EscState::Csi;
                            self.csi.clear();
                            self.csi.push(0x1b);
                            self.csi.push(b'[');
                        } else {
                            // Non-CSI escape (e.g. reverse index `ESC M`) is preserved, so it
                            // scrolls the full screen on this margin-ignoring host.
                            out.push(0x1b);
                            out.push(b);
                            self.state = EscState::Normal;
                        }
                    }
                    EscState::Csi => {
                        self.csi.push(b);
                        if (0x40..=0x7e).contains(&b) {
                            // Final byte reached: drop DECSTBM set/reset margins (`...r`).
                            if b != b'r' {
                                out.extend_from_slice(&self.csi);
                            }
                            self.csi.clear();
                            self.state = EscState::Normal;
                        }
                    }
                }
            }
        }
    }

    impl io::Write for NoScrollRegionBackend {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.raw.extend_from_slice(buf);
            let mut out = Vec::with_capacity(buf.len());
            self.filter(buf, &mut out);
            self.inner.write_all(&out)?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(&mut self.inner)
        }
    }

    impl Backend for NoScrollRegionBackend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.draw(content)
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> io::Result<ratatui::layout::Position> {
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> io::Result<()> {
            self.inner.set_cursor_position(position)
        }
        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear()
        }
        fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> io::Result<()> {
            self.inner.clear_region(clear_type)
        }
        fn append_lines(&mut self, n: u16) -> io::Result<()> {
            self.inner.append_lines(n)
        }
        fn size(&self) -> io::Result<Size> {
            self.inner.size()
        }
        fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
            self.inner.window_size()
        }
        fn flush(&mut self) -> io::Result<()> {
            Backend::flush(&mut self.inner)
        }
        fn scroll_region_up(
            &mut self,
            region: std::ops::Range<u16>,
            scroll_by: u16,
        ) -> io::Result<()> {
            // Not exercised by the insert path under test; delegate unchanged.
            self.inner.scroll_region_up(region, scroll_by)
        }
        fn scroll_region_down(
            &mut self,
            region: std::ops::Range<u16>,
            scroll_by: u16,
        ) -> io::Result<()> {
            self.inner.scroll_region_down(region, scroll_by)
        }
    }

    fn contains_decstbm(bytes: &[u8]) -> bool {
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'r' {
                    return true;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        false
    }

    fn contains_reverse_index(bytes: &[u8]) -> bool {
        bytes.windows(2).any(|w| w == [0x1b, b'M'])
    }

    /// On a host that ignores scroll-region margins, the scroll-region-free path (the one Codex
    /// now selects on Windows) keeps finalized history visible above the composer.
    #[test]
    fn scroll_region_free_path_preserves_history_on_margin_ignoring_host() {
        let width: u16 = 20;
        let height: u16 = 8;
        let mut term = crate::custom_terminal::Terminal::with_options(
            NoScrollRegionBackend::new(width, height),
        )
        .expect("terminal");
        // 1-row composer pinned to the bottom — the common inline layout.
        term.set_viewport_area(Rect::new(0, height - 1, width, 1));

        for i in 0..5 {
            insert_history_lines_with_mode_and_wrap_policy(
                &mut term,
                vec![Line::from(format!("history-line-{i}"))],
                InsertHistoryMode::ZellijRaw,
                HistoryLineWrapPolicy::PreWrap,
            )
            .expect("insert history");
        }

        let rows: Vec<String> = term.backend().vt100().screen().rows(0, width).collect();
        let above: String = rows[..usize::from(term.viewport_area.y)].join("\n");
        let viewport: String = rows[usize::from(term.viewport_area.y)..].join("\n");
        assert!(
            above.contains("history-line-4") && above.contains("history-line-3"),
            "recent history must stay above the viewport on a margin-ignoring host, rows: {rows:?}"
        );
        assert!(
            !viewport.contains("history-line-4"),
            "history must not be written through the composer viewport, rows: {rows:?}"
        );
    }

    /// Pins the root cause: `Standard` relies on DECSTBM margins (and reverse index when the
    /// viewport is mid-screen), while the scroll-region-free path emits neither. This is what
    /// makes the fix portable to hosts that mishandle those margins.
    #[test]
    fn standard_emits_scroll_region_primitives_but_free_path_does_not() {
        let width: u16 = 20;
        let height: u16 = 8;
        // Mid-screen viewport so the Standard path also takes its reverse-index branch.
        let viewport = Rect::new(0, height - 3, width, 1);

        let mut std_term = crate::custom_terminal::Terminal::with_options(
            NoScrollRegionBackend::new(width, height),
        )
        .expect("terminal");
        std_term.set_viewport_area(viewport);
        insert_history_lines_with_mode_and_wrap_policy(
            &mut std_term,
            vec![Line::from("hello")],
            InsertHistoryMode::Standard,
            HistoryLineWrapPolicy::PreWrap,
        )
        .expect("standard insert");
        let std_raw = std_term.backend().raw().to_vec();
        assert!(
            contains_decstbm(&std_raw),
            "Standard path must emit DECSTBM scroll-region margins"
        );
        assert!(
            contains_reverse_index(&std_raw),
            "Standard path with a mid-screen viewport must emit reverse index"
        );

        let mut free_term = crate::custom_terminal::Terminal::with_options(
            NoScrollRegionBackend::new(width, height),
        )
        .expect("terminal");
        free_term.set_viewport_area(viewport);
        insert_history_lines_with_mode_and_wrap_policy(
            &mut free_term,
            vec![Line::from("hello")],
            InsertHistoryMode::ZellijRaw,
            HistoryLineWrapPolicy::PreWrap,
        )
        .expect("free insert");
        let free_raw = free_term.backend().raw().to_vec();
        assert!(
            !contains_decstbm(&free_raw),
            "scroll-region-free path must not emit DECSTBM margins"
        );
        assert!(
            !contains_reverse_index(&free_raw),
            "scroll-region-free path must not emit reverse index"
        );
    }
}
