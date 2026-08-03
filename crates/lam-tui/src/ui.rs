use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, ConversationEntry, EntryKind, Focus, Suggestion};

const ACCENT: Color = Color::Rgb(105, 210, 190);
const DIM: Color = Color::Rgb(112, 118, 128);
const PANEL: Color = Color::Rgb(31, 35, 41);

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
    let model = &app.selected_model().display_name;
    let right = format!("{model}  ·  {}", app.status);
    let right_width = UnicodeWidthStr::width(right.as_str());
    let cwd_space = usize::from(area.width).saturating_sub(right_width + 10);
    let cwd = elide_middle(&app.cwd, cwd_space);
    frame.render_widget(Block::default().style(Style::default().bg(PANEL)), area);
    let left = Line::from(vec![
        Span::styled(
            " λ lam ",
            Style::default().fg(Color::Black).bg(ACCENT).bold(),
        ),
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
    let mut lines = Vec::new();
    let mut ranges = Vec::with_capacity(app.entries.len());
    for (index, entry) in app.entries.iter().enumerate() {
        let start = lines.len();
        entry_lines(
            entry,
            width,
            app.focus == Focus::Conversation && app.selected_entry == Some(index),
            &mut lines,
        );
        let end = lines.len().saturating_sub(1);
        ranges.push((start, end));
    }

    let viewport = usize::from(inner.height);
    let total = lines.len();
    let offset = if app.focus == Focus::Input {
        total.saturating_sub(viewport)
    } else if let Some(selected) = app.selected_entry {
        let (start, end) = ranges[selected];
        if end >= viewport {
            end + 1 - viewport
        } else if start == 0 {
            0
        } else {
            start.min(total.saturating_sub(viewport))
        }
    } else {
        total.saturating_sub(viewport)
    };
    let paragraph = Paragraph::new(Text::from(lines)).scroll((to_u16(offset), 0));
    frame.render_widget(paragraph, inner);

    app.hitboxes.clear();
    for (index, (start, end)) in ranges.into_iter().enumerate() {
        let visible_start = start.max(offset);
        let visible_end = end.min(offset.saturating_add(viewport).saturating_sub(1));
        if visible_start <= visible_end {
            let y_start = inner.y.saturating_add(to_u16(visible_start - offset));
            let y_end = inner.y.saturating_add(to_u16(visible_end - offset));
            app.hitboxes.push((y_start, y_end, index));
        }
    }
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
    let style = if selected {
        Style::default().bg(PANEL)
    } else {
        Style::default()
    };
    let body = one_line(&entry.body);
    let title_width = UnicodeWidthStr::width(entry.title.as_str());
    let fixed = 10 + title_width;
    let preview = if entry.expanded {
        String::new()
    } else {
        elide_end(&body, width.saturating_sub(fixed))
    };
    lines.push(
        Line::from(vec![
            Span::styled(selection.to_owned(), Style::default().fg(ACCENT)),
            Span::styled(
                format!("{disclosure} {marker} "),
                Style::default().fg(color),
            ),
            Span::styled(
                entry.title.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(if preview.is_empty() { "" } else { "  " }),
            Span::styled(preview, Style::default().fg(Color::Gray)),
        ])
        .style(style),
    );
    if entry.expanded {
        let body_width = width.saturating_sub(5).max(1);
        for body_line in wrap_text(&entry.body, body_width) {
            lines.push(
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(body_line, Style::default().fg(Color::Gray)),
                ])
                .style(style),
            );
        }
    }
}

fn render_shelf(frame: &mut Frame<'_>, area: Rect, app: &App, suggestions: &[Suggestion]) {
    let border_style = if app.focus == Focus::Input {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(border_style)
        .title(Span::styled(
            if app.busy { " working " } else { " message " },
            border_style,
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let input_width = usize::from(inner.width.saturating_sub(4).max(1));
    let input_rows = wrap_input(&app.input.text, input_width);
    let palette_height = inner.height.saturating_sub(to_u16(input_rows.len()));
    let [palette_area, input_area] =
        Layout::vertical([Constraint::Length(palette_height), Constraint::Min(1)]).areas(inner);
    if !suggestions.is_empty() && palette_area.height > 0 {
        render_palette(frame, palette_area, suggestions, app.suggestion_index);
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
        let before = app.input.text_before_cursor();
        let display_width = UnicodeWidthStr::width(before.as_str());
        let row = display_width / input_width;
        let column = display_width % input_width;
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
        let available = usize::from(area.width).saturating_sub(6 + suggestion.label.len());
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
    let input_rows = wrap_input(&app.input.text, input_width).len();
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
    let desired = 1 + input_rows + palette_rows;
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

fn wrap_input(text: &str, width: usize) -> Vec<String> {
    let mut lines = wrap_text(text, width);
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
    use super::{elide_end, wrap_text};

    #[test]
    fn wraps_wide_characters_by_terminal_cells() {
        assert_eq!(wrap_text("ab界cd", 4), ["ab界", "cd"]);
    }

    #[test]
    fn elides_to_the_requested_width() {
        assert_eq!(elide_end("abcdefgh", 5), "abcd…");
    }
}
