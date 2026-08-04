use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_markdown::{Options as MarkdownOptions, StyleSheet, from_str_with_options};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, ConversationEntry, EntryKind, EntryLayout, Focus, Hitbox, LayoutKey, Suggestion,
};

const ACCENT: Color = Color::Rgb(105, 210, 190);
const DIM: Color = Color::Rgb(112, 118, 128);
const PANEL: Color = Color::Rgb(31, 35, 41);
const MAX_PENDING_STEER_ROWS: usize = 3;

#[derive(Clone, Copy)]
struct MarkdownStyle;

impl StyleSheet for MarkdownStyle {
    fn heading(&self, level: u8) -> Style {
        match level {
            1 => Style::default().fg(ACCENT).bold(),
            2 => Style::default().fg(Color::White).bold(),
            _ => Style::default().fg(Color::Gray).bold(),
        }
    }

    fn code(&self) -> Style {
        Style::default().fg(Color::LightCyan).bg(PANEL)
    }

    fn link(&self) -> Style {
        Style::default().fg(ACCENT).underlined()
    }

    fn blockquote(&self) -> Style {
        Style::default().fg(DIM).italic()
    }

    fn heading_meta(&self) -> Style {
        Style::default().fg(DIM)
    }

    fn metadata_block(&self) -> Style {
        Style::default().fg(DIM)
    }

    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    fn code_block_fence(&self) -> &str {
        ""
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width < 24 || area.height < 6 {
        frame.render_widget(
            Paragraph::new("lam needs a little more room")
                .style(Style::default().fg(ACCENT))
                .centered(),
            area,
        );
        return;
    }

    let suggestions = app.suggestions();
    let shelf_height = shelf_height(area, app, &suggestions);
    let [header, conversation, shelf] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(shelf_height),
    ])
    .areas(area);
    render_header(frame, header, app);
    render_conversation(frame, conversation, app);
    render_shelf(frame, shelf, app, &suggestions);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let model = app
        .current_agent_model()
        .and_then(|id| app.models.iter().find(|model| model.registry_id == id))
        .map_or_else(
            || app.selected_model().display_name.as_str(),
            |model| model.display_name.as_str(),
        );
    let effort = app.current_agent_effort().unwrap_or("—");
    let right = format!(
        "{}  ·  {model}  ·  {effort} effort  ·  #{}  ·  {}",
        app.current_agent, app.session_id, app.status
    );
    let right_width = UnicodeWidthStr::width(right.as_str());
    let cwd_space = usize::from(area.width).saturating_sub(right_width + 10);
    let cwd = elide_middle(&app.cwd, cwd_space);
    frame.render_widget(Block::default().style(Style::default().bg(PANEL)), area);
    let left = Line::from(vec![
        Span::styled(" λ ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
        Span::styled(cwd, Style::default().fg(DIM)),
    ]);
    frame.render_widget(Paragraph::new(left).style(Style::default().bg(PANEL)), area);
    frame.render_widget(
        Paragraph::new(Span::styled(right, Style::default().fg(Color::Gray)))
            .alignment(Alignment::Right)
            .style(Style::default().bg(PANEL)),
        area,
    );
}

fn render_conversation(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let inner = Rect {
        x: area.x.saturating_add(2),
        y: area.y,
        width: area.width.saturating_sub(4),
        height: area.height,
    };
    let width = usize::from(inner.width.max(1));

    // Pass 1: refresh stale row layouts and measure. Committed rows lay out
    // once and are pure integer work afterwards, so this walk stays cheap on
    // long sessions.
    let selected_index = (app.focus == Focus::Conversation)
        .then_some(app.selected_entry)
        .flatten();
    let mut ranges = Vec::with_capacity(app.entries.len());
    let mut total = 0usize;
    let mut previous_kind = None;
    for (index, entry) in app.entries.iter_mut().enumerate() {
        if needs_message_spacing(previous_kind, Some(entry.kind)) {
            total += 1;
        }
        ensure_entry_layout(entry, width, selected_index == Some(index));
        let height = entry
            .layout
            .as_ref()
            .expect("the layout was just ensured")
            .lines
            .len();
        ranges.push((total, total + height.saturating_sub(1)));
        total += height;
        previous_kind = Some(entry.kind);
    }
    if needs_message_spacing(previous_kind, None) {
        total += 1;
    }

    let viewport = usize::from(inner.height);
    app.conversation_ranges.clone_from(&ranges);
    app.conversation_viewport_height = viewport;
    app.conversation_total_lines = total;
    let selected_start = app.selection_drives_viewport.then(|| {
        app.selected_entry
            .and_then(|selected| ranges.get(selected))
            .map(|(start, _)| *start)
    });
    let offset = viewport_offset(
        app.conversation_offset,
        app.focus == Focus::Input || app.follow_conversation_tail,
        selected_start.flatten(),
        total,
        viewport,
    );
    app.conversation_offset = offset;

    // Pass 2: materialize only the lines inside the viewport window.
    let window_end = offset.saturating_add(viewport);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport);
    let mut cursor = 0usize;
    let mut previous_kind = None;
    for entry in &app.entries {
        if cursor >= window_end {
            break;
        }
        if needs_message_spacing(previous_kind, Some(entry.kind)) {
            if cursor >= offset {
                lines.push(Line::default());
            }
            cursor += 1;
        }
        let cached = &entry
            .layout
            .as_ref()
            .expect("pass 1 ensured every layout")
            .lines;
        let height = cached.len();
        if cursor.saturating_add(height) > offset && cursor < window_end {
            let from = offset.saturating_sub(cursor);
            let to = (window_end - cursor).min(height);
            lines.extend(cached[from..to].iter().cloned());
        }
        cursor += height;
        previous_kind = Some(entry.kind);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    app.hitboxes.clear();
    for (index, (start, end)) in ranges.into_iter().enumerate() {
        let visible_start = start.max(offset);
        let visible_end = end.min(offset.saturating_add(viewport).saturating_sub(1));
        if visible_start <= visible_end {
            let y_start = inner.y.saturating_add(to_u16(visible_start - offset));
            let y_end = inner.y.saturating_add(to_u16(visible_end - offset));
            // The header is the entry's first layout row and the only row
            // that toggles expand/collapse. It is clickable only while it
            // remains inside the viewport window; body clicks just select.
            let header = (start >= offset).then_some(y_start);
            app.hitboxes.push(Hitbox {
                top: y_start,
                bottom: y_end,
                header,
                entry: index,
            });
        }
    }
}

/// Rebuilds the row's cached layout if anything it depends on changed.
fn ensure_entry_layout(entry: &mut ConversationEntry, width: usize, selected: bool) {
    let key = LayoutKey {
        width,
        expanded: entry.expanded,
        selected,
        dimmed: is_dimmed(entry, selected),
        title_len: entry.title.len(),
        body_len: entry.body.len(),
    };
    if entry
        .layout
        .as_ref()
        .is_some_and(|layout| layout.key == key)
    {
        return;
    }
    let mut lines = Vec::new();
    entry_lines(entry, width, selected, &mut lines);
    entry.layout = Some(EntryLayout { key, lines });
}

/// Tool and reasoning rows are ambient detail: they render faint once the
/// stream moves past them. The row a run is actively writing and the
/// selected row stay at full intensity for reading.
fn is_dimmed(entry: &ConversationEntry, selected: bool) -> bool {
    matches!(
        entry.kind,
        EntryKind::ToolCall | EntryKind::ToolResult | EntryKind::Reasoning
    ) && !selected
        && !entry.streaming
}

fn needs_message_spacing(previous: Option<EntryKind>, next: Option<EntryKind>) -> bool {
    previous.is_some_and(is_text_message) || next.is_some_and(is_text_message)
}

fn is_text_message(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::User | EntryKind::Assistant)
}

fn viewport_offset(
    current: usize,
    follow_tail: bool,
    selected_start: Option<usize>,
    total: usize,
    viewport: usize,
) -> usize {
    let viewport = viewport.max(1);
    let maximum = total.saturating_sub(viewport);
    if follow_tail {
        return maximum;
    }

    let mut offset = current.min(maximum);
    if let Some(selected) = selected_start {
        if selected < offset {
            offset = selected;
        } else if selected >= offset.saturating_add(viewport) {
            offset = selected.saturating_add(1).saturating_sub(viewport);
        }
    }
    offset.min(maximum)
}

fn entry_lines(
    entry: &ConversationEntry,
    width: usize,
    selected: bool,
    lines: &mut Vec<Line<'static>>,
) {
    let (marker, color) = entry_style(entry.kind);
    let selection = if selected { "│" } else { " " };
    let disclosure = if entry.expanded { "▾" } else { "▸" };
    let dimmed = is_dimmed(entry, selected);
    let style = if selected {
        Style::default().bg(PANEL)
    } else {
        Style::default()
    };
    let title_width = UnicodeWidthStr::width(entry.title.as_str());
    let fixed = 10 + title_width;
    let preview = if entry.expanded || entry.kind == EntryKind::ToolCall {
        String::new()
    } else {
        let body = if renders_markdown(entry.kind) {
            markdown_preview(&entry.body)
        } else {
            one_line(&entry.body)
        };
        elide_end(&body, width.saturating_sub(fixed))
    };
    lines.push(
        Line::from(vec![
            Span::styled(selection.to_owned(), Style::default().fg(ACCENT)),
            Span::styled(
                format!("{disclosure} {marker} "),
                dim_ambient(style_fg(color), dimmed),
            ),
            Span::styled(
                entry.title.clone(),
                if dimmed {
                    style_fg(color).add_modifier(Modifier::DIM)
                } else {
                    style_fg(color).add_modifier(Modifier::BOLD)
                },
            ),
            Span::raw(if preview.is_empty() { "" } else { "  " }),
            Span::styled(preview, dim_ambient(style_fg(Color::Gray), dimmed)),
        ])
        .style(style),
    );
    if entry.expanded {
        let body_width = width.saturating_sub(5).max(1);
        if renders_markdown(entry.kind) {
            let base = dim_ambient(style_fg(Color::Gray), dimmed);
            for body_line in markdown_lines(&entry.body, body_width, base) {
                let mut spans = Vec::with_capacity(body_line.spans.len() + 1);
                spans.push(Span::raw("    "));
                spans.extend(body_line.spans);
                lines.push(Line::from(spans).style(style));
            }
        } else {
            for body_line in wrap_text(&entry.body, body_width) {
                lines.push(
                    Line::from(vec![
                        Span::raw("    "),
                        Span::styled(body_line, dim_ambient(style_fg(Color::Gray), dimmed)),
                    ])
                    .style(style),
                );
            }
        }
    }
}

fn style_fg(color: Color) -> Style {
    Style::default().fg(color)
}

fn dim_ambient(style: Style, dimmed: bool) -> Style {
    if dimmed {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

fn renders_markdown(kind: EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::User | EntryKind::Assistant | EntryKind::Reasoning
    )
}

fn markdown_lines(markdown: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let options = MarkdownOptions::new(MarkdownStyle);
    let text = from_str_with_options(markdown, &options);
    wrap_markdown_lines(text.lines, width, base)
}

fn markdown_preview(markdown: &str) -> String {
    let options = MarkdownOptions::new(MarkdownStyle);
    one_line(&from_str_with_options(markdown, &options).to_string())
}

fn wrap_markdown_lines(lines: Vec<Line<'_>>, width: usize, base: Style) -> Vec<Line<'static>> {
    let mut output = Vec::new();
    let mut lines = lines.into_iter().peekable();
    while let Some(line) = lines.next() {
        if line.to_string().starts_with('┌') {
            let mut table = vec![line];
            while let Some(next) = lines.next_if(|next| !next.to_string().starts_with('└')) {
                table.push(next);
            }
            if let Some(bottom) = lines.next() {
                table.push(bottom);
            }
            if table
                .last()
                .is_some_and(|bottom| bottom.to_string().starts_with('└'))
                && let Some(wrapped) = wrap_markdown_table(table.clone(), width, base)
            {
                output.extend(wrapped);
                continue;
            }
            for line in table {
                output.extend(wrap_styled_line(line, width, base));
            }
        } else {
            output.extend(wrap_styled_line(line, width, base));
        }
    }
    output
}

fn wrap_markdown_table(
    table: Vec<Line<'_>>,
    width: usize,
    base: Style,
) -> Option<Vec<Line<'static>>> {
    let desired = table_column_widths(table.first()?.to_string().as_str())?;
    let overhead = desired.len().saturating_mul(3).saturating_add(1);
    let available = width.checked_sub(overhead)?;
    if available < desired.len() {
        return None;
    }
    let widths = constrain_column_widths(&desired, available);
    let mut output = Vec::new();
    for line in table {
        let text = line.to_string();
        let border_style = line
            .spans
            .first()
            .map_or(line.style, |span| line.style.patch(span.style));
        match text.chars().next() {
            Some('┌') => output.push(table_border_line('┌', '┬', '┐', &widths, border_style)),
            Some('├') => output.push(table_border_line('├', '┼', '┤', &widths, border_style)),
            Some('└') => output.push(table_border_line('└', '┴', '┘', &widths, border_style)),
            Some('│') => output.extend(wrap_table_row(line, &widths, base)?),
            _ => return None,
        }
    }
    Some(output)
}

fn table_column_widths(top_border: &str) -> Option<Vec<usize>> {
    let top_border = top_border.strip_prefix('┌')?.strip_suffix('┐')?;
    let widths = top_border
        .split('┬')
        .map(|segment| UnicodeWidthStr::width(segment).checked_sub(2))
        .collect::<Option<Vec<_>>>()?;
    (!widths.is_empty()).then_some(widths)
}

fn constrain_column_widths(desired: &[usize], available: usize) -> Vec<usize> {
    if desired.iter().sum::<usize>() <= available {
        return desired.to_vec();
    }
    let mut widths = vec![1; desired.len()];
    let mut remaining = available.saturating_sub(widths.len());
    while remaining > 0 {
        let mut advanced = false;
        for (width, desired) in widths.iter_mut().zip(desired) {
            if *width < *desired {
                *width += 1;
                remaining -= 1;
                advanced = true;
                if remaining == 0 {
                    break;
                }
            }
        }
        if !advanced {
            break;
        }
    }
    widths
}

fn table_border_line(
    left: char,
    intersection: char,
    right: char,
    widths: &[usize],
    style: Style,
) -> Line<'static> {
    let mut border = String::from(left);
    for (index, width) in widths.iter().enumerate() {
        border.push_str(&"─".repeat(width + 2));
        border.push(if index + 1 == widths.len() {
            right
        } else {
            intersection
        });
    }
    Line::from(Span::styled(border, style))
}

fn wrap_table_row(line: Line<'_>, widths: &[usize], base: Style) -> Option<Vec<Line<'static>>> {
    let border_style = line
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "│")
        .map_or(line.style, |span| line.style.patch(span.style));
    let mut cells = Vec::new();
    let mut cell = Vec::new();
    let mut inside = false;
    for span in line.spans {
        if span.content.as_ref() == "│" {
            if inside {
                cells.push(trim_cell_spans(std::mem::take(&mut cell)));
            }
            inside = true;
        } else if inside {
            cell.push(Span::styled(
                span.content.into_owned(),
                line.style.patch(span.style),
            ));
        }
    }
    if cells.len() != widths.len() {
        return None;
    }
    let cells = cells
        .into_iter()
        .zip(widths)
        .map(|(spans, width)| wrap_styled_words(Line::from(spans), *width, base))
        .collect::<Vec<_>>();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    let mut rows = Vec::with_capacity(height);
    for row in 0..height {
        let mut spans = vec![Span::styled("│", border_style)];
        for (column, width) in widths.iter().enumerate() {
            spans.push(Span::raw(" "));
            if let Some(line) = cells[column].get(row) {
                let used = line.width();
                spans.extend(line.spans.clone());
                spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
            } else {
                spans.push(Span::raw(" ".repeat(*width)));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled("│", border_style));
        }
        rows.push(Line::from(spans));
    }
    Some(rows)
}

fn trim_cell_spans(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    while spans
        .first()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.remove(0);
    }
    while spans
        .last()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.pop();
    }
    if let Some(first) = spans.first_mut() {
        first.content = first.content.trim_start().to_owned().into();
    }
    if let Some(last) = spans.last_mut() {
        last.content = last.content.trim_end().to_owned().into();
    }
    spans
}

fn wrap_styled_words(line: Line<'_>, width: usize, base: Style) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    let line_style = base.patch(line.style);
    let characters = line
        .spans
        .into_iter()
        .flat_map(|span| {
            let style = line_style.patch(span.style);
            span.content
                .into_owned()
                .chars()
                .map(move |character| (character, style))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    if characters.is_empty() {
        return vec![Line::default()];
    }

    let mut lines = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        while start < characters.len() && characters[start].0.is_whitespace() {
            start += 1;
        }
        if start == characters.len() {
            break;
        }
        let mut end = start;
        let mut used = 0;
        while end < characters.len() {
            let character_width = characters[end].0.width().unwrap_or(0);
            if end > start && used + character_width > width {
                break;
            }
            used += character_width;
            end += 1;
            if used >= width {
                break;
            }
        }
        let cut = if end < characters.len() {
            characters[start..end]
                .iter()
                .rposition(|(character, _)| character.is_whitespace())
                .map_or(end, |space| start + space)
        } else {
            end
        };
        let cut = cut.max(start + 1);
        lines.push(styled_character_line(&characters[start..cut]));
        start = if cut < end { cut + 1 } else { end };
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn styled_character_line(characters: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (character, style) in characters {
        if let Some(span) = spans.last_mut()
            && span.style == *style
        {
            span.content.to_mut().push(*character);
        } else {
            spans.push(Span::styled(character.to_string(), *style));
        }
    }
    Line::from(spans)
}

fn wrap_styled_line(line: Line<'_>, width: usize, base: Style) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }

    let line_style = base.patch(line.style);
    let mut wrapped = Vec::new();
    let mut spans = Vec::new();
    let mut line_width = 0;

    for span in line.spans {
        let style = line_style.patch(span.style);
        let mut fragment = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if line_width > 0 && line_width + character_width > width {
                if !fragment.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut fragment), style));
                }
                wrapped.push(Line::from(std::mem::take(&mut spans)));
                line_width = 0;
            }
            fragment.push(character);
            line_width += character_width;
        }
        if !fragment.is_empty() {
            spans.push(Span::styled(fragment, style));
        }
    }

    wrapped.push(Line::from(spans));
    wrapped
}

fn render_shelf(frame: &mut Frame<'_>, area: Rect, app: &mut App, suggestions: &[Suggestion]) {
    let border_style = if app.focus == Focus::Input {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    // ` message ` and ` working ` are the same width, so the context
    // parenthetical stays in the same column across both states.
    let mut title = if app.busy {
        " working ".to_owned()
    } else {
        " message ".to_owned()
    };
    if let Some(consumed) = app.context_tokens
        && let Some(window) = app.context_window_tokens()
        && window > 0
    {
        let consumed = consumed.min(window);
        let percent = consumed.saturating_mul(100) / window.max(1);
        title.push_str(&format!(
            "({percent}% · {}k/{}k) ",
            consumed.saturating_add(500) / 1000,
            window.saturating_add(500) / 1000,
        ));
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(border_style)
        .title(Span::styled(title, border_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let input_width = usize::from(inner.width.saturating_sub(4).max(1));
    app.input_width = input_width;
    let input_rows = app.input.rows(input_width);

    // Allocate by priority: the input keeps every row it needs, then the
    // warning, then pending steers, and the palette takes the remainder.
    let inner_height = usize::from(inner.height);
    let input_height = input_rows.len().clamp(1, inner_height.max(1));
    let remaining = inner_height.saturating_sub(input_height);
    let warning_height = usize::from(app.interruption_warning().is_some()).min(remaining);
    let remaining = remaining.saturating_sub(warning_height);
    let steer_height = app
        .pending_steers
        .len()
        .min(MAX_PENDING_STEER_ROWS)
        .min(remaining);
    let palette_height = remaining.saturating_sub(steer_height);
    let [palette_area, steer_area, warning_area, input_area] = Layout::vertical([
        Constraint::Length(to_u16(palette_height)),
        Constraint::Length(to_u16(steer_height)),
        Constraint::Length(to_u16(warning_height)),
        Constraint::Min(1),
    ])
    .areas(inner);
    app.input_area = Some(input_area);
    if steer_height > 0 {
        render_pending_steers(frame, steer_area, app);
    }
    if !suggestions.is_empty() && palette_area.height > 0 {
        render_palette(frame, palette_area, suggestions, app.suggestion_index);
    }
    if let Some(warning) = app.interruption_warning() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("   ! ", Style::default().fg(Color::Yellow).bold()),
                Span::styled(warning, Style::default().fg(Color::Yellow)),
            ])),
            warning_area,
        );
    }

    let input_lines = input_rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::from(vec![
                Span::styled(
                    if index == 0 { " › " } else { "   " },
                    Style::default().fg(ACCENT),
                ),
                Span::raw(row),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(input_lines), input_area);

    if app.focus == Focus::Input {
        let (row, column) = app.input.cursor_position(input_width);
        let cursor_y = input_area
            .y
            .saturating_add(to_u16(row).min(input_area.height.saturating_sub(1)));
        frame.set_cursor_position((
            input_area
                .x
                .saturating_add(3)
                .saturating_add(to_u16(column)),
            cursor_y,
        ));
    }
}

fn render_pending_steers(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let prefix_width = UnicodeWidthStr::width("   pending steer  ");
    let preview_width = usize::from(area.width).saturating_sub(prefix_width);
    let capacity = usize::from(area.height);
    let pending = app.pending_steers.len();
    // When the list does not fit, the last row becomes a "+N more" marker so
    // truncation is never silent.
    let shown = if pending > capacity {
        capacity.saturating_sub(1)
    } else {
        pending
    };
    let mut lines = app
        .pending_steers
        .iter()
        .take(shown)
        .map(|steer| {
            Line::from(vec![
                Span::styled("   ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    "pending steer",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    elide_end(&one_line(&steer.text), preview_width),
                    Style::default().fg(Color::Gray),
                ),
            ])
        })
        .collect::<Vec<_>>();
    if pending > shown {
        lines.push(Line::from(Span::styled(
            format!("   +{} more queued", pending - shown),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_palette(frame: &mut Frame<'_>, area: Rect, suggestions: &[Suggestion], selected: usize) {
    let mut lines = Vec::new();
    let mut item_lines = Vec::with_capacity(suggestions.len());
    let mut provider = None;
    for (index, suggestion) in suggestions.iter().enumerate() {
        if suggestion.provider.as_deref() != provider {
            provider = suggestion.provider.as_deref();
            if let Some(provider) = provider {
                lines.push(Line::from(Span::styled(
                    format!("   {provider}"),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                )));
            }
        }
        item_lines.push(lines.len());
        let style = if index == selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default().fg(Color::White)
        };
        let label_width = UnicodeWidthStr::width(suggestion.label.as_str());
        let available = usize::from(area.width).saturating_sub(6 + label_width);
        lines.push(
            Line::from(vec![
                Span::styled(if index == selected { " › " } else { "   " }, style),
                Span::styled(suggestion.label.clone(), style.add_modifier(Modifier::BOLD)),
                Span::styled("  ", style),
                Span::styled(elide_end(&suggestion.detail, available), style),
            ])
            .style(style),
        );
    }
    let selected_line = item_lines[selected];
    let height = usize::from(area.height);
    let offset = selected_line.saturating_sub(height.saturating_sub(1));
    frame.render_widget(Paragraph::new(lines).scroll((to_u16(offset), 0)), area);
}

fn shelf_height(area: Rect, app: &App, suggestions: &[Suggestion]) -> u16 {
    let input_width = usize::from(area.width.saturating_sub(6).max(1));
    let input_rows = app.input.rows(input_width).len();
    let provider_headers = suggestions
        .iter()
        .filter_map(|suggestion| suggestion.provider.as_deref())
        .fold((0, None), |(count, previous), provider| {
            (
                count + usize::from(previous != Some(provider)),
                Some(provider),
            )
        })
        .0;
    let palette_rows = suggestions.len() + provider_headers;
    let warning_rows = usize::from(app.interruption_warning().is_some());
    let steer_rows = app.pending_steers.len().min(MAX_PENDING_STEER_ROWS);
    let desired = 1 + input_rows + steer_rows + palette_rows + warning_rows;
    let maximum = usize::from((area.height * 2 / 5).max(3));
    to_u16(desired.clamp(2, maximum))
}

fn entry_style(kind: EntryKind) -> (&'static str, Color) {
    match kind {
        EntryKind::User => ("you", ACCENT),
        EntryKind::Assistant => ("agent", Color::White),
        EntryKind::Reasoning => ("think", Color::Magenta),
        EntryKind::ToolCall => ("call", Color::Yellow),
        EntryKind::ToolResult => ("result", Color::Green),
        EntryKind::System => ("info", DIM),
        EntryKind::Error => ("error", Color::Red),
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for source_line in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0;
        for character in source_line.chars() {
            let character_width = character.width().unwrap_or(0);
            if line_width > 0 && line_width + character_width > width {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width += character_width;
        }
        lines.push(line);
    }
    lines
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn elide_end(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width + 1 > width {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}

fn elide_middle(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return elide_end(text, width);
    }
    let left_width = (width - 1) / 2;
    let right_width = width - left_width - 1;
    let left = elide_end(text, left_width + 1)
        .trim_end_matches('…')
        .to_owned();
    let mut right = String::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > right_width {
            break;
        }
        right.insert(0, character);
        used += character_width;
    }
    format!("{left}…{right}")
}

fn to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lam_agents::{ActorAddress, AgentSystemEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier, Style};

    use super::{
        PANEL, elide_end, markdown_lines, markdown_preview, needs_message_spacing, render,
        renders_markdown, viewport_offset, wrap_text,
    };
    use crate::app::{App, EntryKind, Focus, SessionChoice, SessionView};
    use crate::config::ModelChoice;
    use crate::runtime::{AgentHistory, CommittedRow, HistoryEntry, HistoryKind};

    fn test_app(history: Vec<HistoryEntry>) -> App {
        let history = history
            .into_iter()
            .map(|entry| CommittedRow {
                entry,
                run_id: None,
            })
            .collect();
        App::new(
            "/tmp/project".to_owned(),
            "/tmp/providers.toml".to_owned(),
            SessionView {
                id: 1,
                journal_path: "/tmp/session.redb".to_owned(),
                resumed: false,
                agents: vec![AgentHistory::root(history)],
                choices: vec![SessionChoice {
                    id: 1,
                    preview: None,
                }],
            },
            vec![ModelChoice {
                registry_id: "openai/gpt-5".to_owned(),
                provider: "openai".to_owned(),
                model: "gpt-5".to_owned(),
                display_name: "GPT-5".to_owned(),
                context_window: 400_000,
                efforts: vec!["low".to_owned(), "high".to_owned()],
            }],
            0,
            "high",
        )
    }

    #[test]
    fn wraps_wide_characters_by_terminal_cells() {
        assert_eq!(wrap_text("ab界cd", 4), ["ab界", "cd"]);
    }

    #[test]
    fn elides_to_the_requested_width() {
        assert_eq!(elide_end("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn markdown_preserves_inline_styles_in_ratatui_spans() {
        let lines = markdown_lines(
            "A **bold** word and `code`.",
            80,
            Style::default().fg(Color::Gray),
        );
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();

        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<String>(),
            "A bold word and code."
        );
        assert!(spans.iter().any(|span| {
            span.content.contains("bold")
                && span
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD)
        }));
        assert!(
            spans
                .iter()
                .any(|span| { span.content.contains("code") && span.style.bg == Some(PANEL) })
        );
    }

    #[test]
    fn markdown_wraps_styled_wide_text_by_terminal_cells() {
        let lines = markdown_lines("**ab界cd**", 4, Style::default().fg(Color::Gray));

        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["ab界", "cd"]
        );
        assert!(lines.iter().all(|line| line.width() <= 4));
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        }));
    }

    #[test]
    fn markdown_tables_wrap_cells_within_the_pane() {
        let markdown = "| Term | Meaning |\n| --- | --- |\n| **Thread-mobile / migratable** | The value can be moved across threads while work is running. |\n| **Thread-affine** | The value must remain on the worker that owns it. |";
        let lines = markdown_lines(markdown, 42, Style::default().fg(Color::Gray));
        let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
        let rows = rendered
            .iter()
            .filter(|line| line.starts_with('│'))
            .collect::<Vec<_>>();

        assert!(lines.iter().all(|line| line.width() <= 42));
        assert!(rows.len() > 3, "cells should grow table rows vertically");
        assert!(
            rows.iter()
                .all(|line| line.ends_with('│') && line.matches('│').count() == 3)
        );
        assert!(rendered.iter().any(|line| line.contains("Thread-mobile")));
        assert!(rendered.iter().any(|line| line.contains("across")));
        assert!(rendered.iter().any(|line| line.contains("threads")));
    }

    #[test]
    fn collapsed_markdown_preview_removes_presentation_syntax() {
        assert_eq!(
            markdown_preview("# Result\n\nUse **care** with `code`."),
            "Result Use care with code."
        );
    }

    #[test]
    fn markdown_is_scoped_to_conversational_text_and_reasoning() {
        assert!(renders_markdown(EntryKind::User));
        assert!(renders_markdown(EntryKind::Assistant));
        assert!(renders_markdown(EntryKind::Reasoning));
        assert!(!renders_markdown(EntryKind::ToolCall));
        assert!(!renders_markdown(EntryKind::ToolResult));
        assert!(!renders_markdown(EntryKind::System));
        assert!(!renders_markdown(EntryKind::Error));
    }

    #[test]
    fn text_message_spacing_is_deduplicated_at_shared_boundaries() {
        assert!(needs_message_spacing(None, Some(EntryKind::User)));
        assert!(needs_message_spacing(
            Some(EntryKind::User),
            Some(EntryKind::Assistant)
        ));
        assert!(needs_message_spacing(
            Some(EntryKind::Assistant),
            Some(EntryKind::ToolCall)
        ));
        assert!(needs_message_spacing(
            Some(EntryKind::ToolResult),
            Some(EntryKind::User)
        ));
        assert!(needs_message_spacing(Some(EntryKind::Assistant), None));
        assert!(!needs_message_spacing(
            Some(EntryKind::ToolCall),
            Some(EntryKind::ToolResult)
        ));
        assert!(!needs_message_spacing(Some(EntryKind::ToolResult), None));
    }

    #[test]
    fn conversation_geometry_includes_one_blank_line_around_text_messages() {
        let mut app = test_app(vec![
            HistoryEntry {
                kind: HistoryKind::User,
                title: "You".to_owned(),
                body: "First prompt".to_owned(),
            },
            HistoryEntry {
                kind: HistoryKind::Assistant,
                title: "/root".to_owned(),
                body: "First answer".to_owned(),
            },
        ]);
        app.entries.pop();
        app.selected_entry = Some(1);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.conversation_total_lines, 7);
        assert_eq!(app.conversation_ranges, [(1, 2), (4, 5)]);
    }

    #[test]
    fn incomplete_streamed_code_fence_renders_its_content() {
        let lines = markdown_lines(
            "Working…\n\n```rust\nlet answer = 42;",
            80,
            Style::default().fg(Color::Gray),
        );
        let rendered = lines.iter().map(ToString::to_string).collect::<String>();

        assert!(rendered.contains("Working…"));
        assert!(rendered.contains("let answer = 42;"));
        assert!(!rendered.contains("```"));
        assert!(
            lines.iter().flat_map(|line| &line.spans).any(|span| {
                span.content.contains("let answer") && span.style.bg == Some(PANEL)
            })
        );
    }

    #[test]
    fn detached_viewport_ignores_new_content_below_it() {
        assert_eq!(viewport_offset(4, false, Some(6), 20, 10), 4);
        assert_eq!(viewport_offset(4, false, Some(6), 80, 10), 4);
        assert_eq!(viewport_offset(4, true, Some(6), 80, 10), 70);
    }

    #[test]
    fn detached_viewport_scrolls_only_to_reveal_the_selected_header() {
        assert_eq!(viewport_offset(10, false, Some(7), 40, 10), 7);
        assert_eq!(viewport_offset(10, false, Some(24), 40, 10), 15);
        assert_eq!(viewport_offset(10, false, Some(12), 80, 10), 10);
    }

    #[test]
    fn rendered_header_tracks_the_selected_agent() {
        let mut app = test_app(Vec::new());
        app.apply_agent_event(AgentSystemEvent::Hosted {
            address: ActorAddress::new("/root/worker").unwrap(),
            parent: Some(ActorAddress::new("/root").unwrap()),
        });
        app.input.text = "/agents /root/worker".to_owned();
        app.input.cursor = app.input.char_count();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("/root/worker"));
    }

    #[test]
    fn header_shows_lambda_and_cwd_without_the_wordmark() {
        let mut app = test_app(Vec::new());
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("λ /tmp/project"));
        assert!(!rendered.contains("λ lam"));
    }

    #[test]
    fn message_bar_reports_context_consumption() {
        let mut app = test_app(Vec::new());
        app.context_tokens = Some(123_456);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(" message (30% · 123k/400k) "));
    }

    #[test]
    fn message_bar_omits_context_before_the_first_model_turn() {
        let mut app = test_app(Vec::new());
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(" message "));
        assert!(!rendered.contains("tokens)"));
    }

    #[test]
    fn message_bar_keeps_context_while_working() {
        let mut app = test_app(Vec::new());
        app.context_tokens = Some(123_456);
        app.busy = true;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(" working (30% · 123k/400k) "));
    }

    #[test]
    fn context_parenthetical_stays_aligned_between_states() {
        let mut app = test_app(Vec::new());
        app.context_tokens = Some(123_456);
        let paren_column = |app: &mut App, label: &str| {
            let backend = TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, app)).unwrap();
            let content = terminal.backend().buffer().content();
            let row = content
                .chunks(100)
                .position(|row| {
                    row.iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .contains(label)
                })
                .expect("the shelf title row should render");
            content[row * 100..(row + 1) * 100]
                .iter()
                .position(|cell| cell.symbol() == "(")
                .expect("the context parenthetical should render")
        };
        let message_column = paren_column(&mut app, " message (");
        app.busy = true;
        let working_column = paren_column(&mut app, " working (");
        assert_eq!(message_column, working_column);
    }

    #[test]
    fn collapsed_eval_rows_hide_source_until_expanded() {
        let mut app = test_app(vec![HistoryEntry {
            kind: HistoryKind::ToolCall,
            title: "/root · Inspect the workspace".to_owned(),
            body: "const secret_marker = await lam.fs.list({ path: '.' });".to_owned(),
        }]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let collapsed = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(collapsed.contains("Inspect the workspace"));
        assert!(!collapsed.contains("secret_marker"));

        app.entries[0].expanded = true;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let expanded = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(expanded.contains("secret_marker"));
    }

    #[test]
    fn tool_rows_render_dimmed_until_selected() {
        let mut app = test_app(vec![HistoryEntry {
            kind: HistoryKind::ToolCall,
            title: "/root · Inspect the workspace".to_owned(),
            body: "const files = await lam.fs.list({ path: '.' });".to_owned(),
        }]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let dimmed = text_modifier(terminal.backend().buffer(), "/root · Inspect");
        assert!(dimmed.contains(Modifier::DIM));
        assert!(!dimmed.contains(Modifier::BOLD));

        app.focus = Focus::Conversation;
        app.selected_entry = Some(0);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let focused = text_modifier(terminal.backend().buffer(), "/root · Inspect");
        assert!(!focused.contains(Modifier::DIM));
        assert!(focused.contains(Modifier::BOLD));
    }

    #[test]
    fn reasoning_rows_render_dimmed_until_selected() {
        let mut app = test_app(vec![HistoryEntry {
            kind: HistoryKind::Reasoning,
            title: "agent".to_owned(),
            body: "weighing the options".to_owned(),
        }]);
        app.entries[0].expanded = true;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(text_modifier(buffer, "weighing").contains(Modifier::DIM));

        app.focus = Focus::Conversation;
        app.selected_entry = Some(0);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(!text_modifier(buffer, "weighing").contains(Modifier::DIM));
    }

    #[test]
    fn streaming_rows_render_at_full_intensity() {
        let mut app = test_app(vec![HistoryEntry {
            kind: HistoryKind::Reasoning,
            title: "agent".to_owned(),
            body: "weighing the options".to_owned(),
        }]);
        app.entries[0].expanded = true;
        app.entries[0].streaming = true;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(!text_modifier(buffer, "weighing").contains(Modifier::DIM));
    }

    fn text_modifier(buffer: &ratatui::buffer::Buffer, needle: &str) -> Modifier {
        let cells = buffer.content();
        let symbols: Vec<&str> = cells.iter().map(|cell| cell.symbol()).collect();
        let needle: Vec<String> = needle
            .chars()
            .map(|character| character.to_string())
            .collect();
        let index = symbols
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
            .expect("the needle is rendered");
        cells[index].modifier
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn layout_cache_reuses_untouched_rows_and_rebuilds_grown_ones() {
        let mut app = test_app(vec![
            HistoryEntry {
                kind: HistoryKind::System,
                title: "Static".to_owned(),
                body: "unchanged content".to_owned(),
            },
            HistoryEntry {
                kind: HistoryKind::Assistant,
                title: "/root".to_owned(),
                body: "growing".to_owned(),
            },
        ]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let static_lines = app.entries[0].layout.as_ref().unwrap().lines.as_ptr();
        let grown_before = app.entries[1].layout.as_ref().unwrap().key.clone();

        app.entries[1].body.push_str(" and growing");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(
            app.entries[0].layout.as_ref().unwrap().lines.as_ptr(),
            static_lines,
            "an untouched row keeps its cached layout allocation"
        );
        let grown_after = &app.entries[1].layout.as_ref().unwrap().key;
        assert!(
            grown_after.body_len > grown_before.body_len,
            "a grown row rebuilds its layout"
        );
        assert!(buffer_text(&terminal).contains("and growing"));
    }

    #[test]
    fn viewport_materializes_only_the_visible_window() {
        let rows = (0..80)
            .map(|index| HistoryEntry {
                kind: HistoryKind::System,
                title: format!("row-{index}"),
                body: format!("body {index}"),
            })
            .collect::<Vec<_>>();
        let mut app = test_app(rows);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        // Following the tail shows the newest rows and not the oldest.
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let tail = buffer_text(&terminal);
        assert!(tail.contains("row-79"));
        assert!(!tail.contains("row-2 "));

        // A detached viewport shows the middle of the scrollback.
        app.follow_conversation_tail = false;
        app.selection_drives_viewport = false;
        app.focus = Focus::Conversation;
        app.conversation_offset = 40;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let middle = buffer_text(&terminal);
        assert!(middle.contains("row-41"));
        assert!(!middle.contains("row-79"));
        assert!(!middle.contains("row-2 "));
    }
}
