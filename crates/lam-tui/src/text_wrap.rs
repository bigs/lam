//! Word-aware soft-wrapping shared by the input editor and the
//! conversation pane.
//!
//! Rows never break mid-word: when the next word does not fit on the
//! current row, the break moves before the word. A word wider than its
//! container is the one exception — it is split, and every continuation
//! row after its first chunk is indented two spaces (clamped when the
//! container is too narrow to hold both the indent and the next item).
//! Items are extended grapheme clusters, so emoji sequences and combining
//! marks are never torn apart by a wrap.

use unicode_width::UnicodeWidthStr;

/// Indent applied to continuation rows of a word wider than its container.
const LONG_WORD_INDENT: usize = 2;

/// Whitespace characters that permit a soft wrap.
///
/// Non-breaking spaces and word joiners stay inside their word; `U+200B`
/// (zero-width space) is an explicit, invisible break opportunity.
pub(crate) fn is_breakable_whitespace(character: char) -> bool {
    character == '\u{200B}' // zero-width space
        || (character.is_whitespace()
            && !matches!(
            character,
            '\u{00A0}'   // no-break space
                | '\u{2007}' // figure space
                | '\u{202F}' // narrow no-break space
                | '\u{2060}' // word joiner
        ))
}

/// One rendered row produced by [`wrap_items`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WrapRow {
    /// Index of the first item rendered on this row.
    pub(crate) start: usize,
    /// Index one past the last item rendered on this row. Items in
    /// `start..end` are displayed; inter-word whitespace omitted at a soft
    /// wrap lives in the gap between consecutive rows' ranges.
    pub(crate) end: usize,
    /// Synthetic leading spaces before the row's content. Only continuation
    /// rows of an over-wide word are indented.
    pub(crate) indent: usize,
}

/// Wraps `items` into rows no wider than `requested_width` display cells.
///
/// `item` projects each item to its text (normally one grapheme cluster)
/// and `breakable_whitespace` decides which items are soft-wrap points.
/// A word (a maximal run of non-breakable items) is never split unless its
/// width exceeds the full container width; the first chunk of such a word
/// fills the row and every later chunk starts on a new row indented
/// [`LONG_WORD_INDENT`] cells. Inter-word whitespace is preserved when the
/// following word fits with it, and omitted at the soft-wrap boundary
/// otherwise. Leading and trailing whitespace are content and are emitted
/// verbatim.
pub(crate) fn wrap_items<T, F, G>(
    items: &[T],
    requested_width: usize,
    item: F,
    breakable_whitespace: G,
) -> Vec<WrapRow>
where
    F: Fn(&T) -> &str,
    G: Fn(&T) -> bool,
{
    let width = requested_width.max(1);
    let continuation_indent = LONG_WORD_INDENT.min(width.saturating_sub(1));
    if items.is_empty() {
        return vec![WrapRow {
            start: 0,
            end: 0,
            indent: 0,
        }];
    }
    let width_of = |position: usize| UnicodeWidthStr::width(item(&items[position]));
    let breakable = |position: usize| breakable_whitespace(&items[position]);

    let mut state = WrapState::default();
    let mut pending_ws = None;
    let mut saw_word = false;

    let mut index = 0;
    while index < items.len() {
        if breakable(index) {
            let ws_start = index;
            while index < items.len() && breakable(index) {
                index += 1;
            }
            if saw_word {
                pending_ws = Some(ws_start);
            } else {
                // Leading whitespace is content: emit it literally, wrapping
                // onto unindented rows when it overflows.
                emit_whitespace(items, &item, ws_start, index, width, &mut state);
            }
            continue;
        }

        let word_start = index;
        while index < items.len() && !breakable(index) {
            index += 1;
        }
        let word_end = index;
        let word_width: usize = (word_start..word_end).map(&width_of).sum();
        saw_word = true;

        if word_width > width {
            // An over-wide word starts on a fresh full-width row and is then
            // split with indented continuation rows.
            state.flush();
            pending_ws = None;
            state.begin(word_start, 0);
            emit_long_word(
                items,
                &item,
                word_start,
                word_end,
                width,
                continuation_indent,
                &mut state,
            );
            continue;
        }

        let separator_width =
            pending_ws.map_or(0, |ws_start| (ws_start..word_start).map(&width_of).sum());
        let available = width.saturating_sub(state.row_indent);
        if state.used + separator_width + word_width <= available {
            if pending_ws.take().is_some() {
                state.used += separator_width;
            }
            state.append(word_end, word_width);
        } else {
            state.flush();
            pending_ws = None;
            state.begin(word_start, 0);
            state.append(word_end, word_width);
        }
    }

    if let Some(ws_start) = pending_ws {
        emit_whitespace(items, &item, ws_start, items.len(), width, &mut state);
    }
    // Trailing rows with no display width (only zero-width items) would be
    // invisible phantom rows; keep at least one row for empty input.
    if (state.row_end > state.row_start && state.used > 0) || state.rows.is_empty() {
        state.rows.push(WrapRow {
            start: state.row_start,
            end: state.row_end,
            indent: state.row_indent,
        });
    }
    state.rows
}

/// Accumulator for the row currently being filled.
#[derive(Default)]
struct WrapState {
    rows: Vec<WrapRow>,
    row_start: usize,
    row_end: usize,
    row_indent: usize,
    used: usize,
}

impl WrapState {
    /// Pushes the open row when it holds any items.
    fn flush(&mut self) {
        if self.row_end > self.row_start && self.used > 0 {
            self.rows.push(WrapRow {
                start: self.row_start,
                end: self.row_end,
                indent: self.row_indent,
            });
        }
    }

    /// Opens a fresh empty row starting at item `start`.
    fn begin(&mut self, start: usize, indent: usize) {
        self.row_start = start;
        self.row_end = start;
        self.row_indent = indent;
        self.used = 0;
    }

    /// Extends the open row through item `end`, adding `width` cells.
    fn append(&mut self, end: usize, width: usize) {
        self.row_end = end;
        self.used += width;
    }
}

/// Splits one word wider than the container. The first chunk fills the
/// current (empty) row; each later chunk starts a new row indented by
/// `continuation_indent` cells.
///
/// The continuation indent is clamped per chunk: when the next item alone
/// would not fit beside the full indent, the indent shrinks (or vanishes)
/// so the row never exceeds the container width. Only an item wider than
/// the whole container may overflow, because a single item cannot be split.
fn emit_long_word<T, F>(
    items: &[T],
    item: &F,
    word_start: usize,
    word_end: usize,
    width: usize,
    continuation_indent: usize,
    state: &mut WrapState,
) where
    F: Fn(&T) -> &str,
{
    let width_of = |position: usize| UnicodeWidthStr::width(item(&items[position]));
    let mut capacity = width;
    let mut indent = 0;
    let mut position = word_start;
    while position < word_end {
        if position > word_start {
            indent = continuation_indent.min(width.saturating_sub(width_of(position)));
            capacity = width.saturating_sub(indent);
        }
        let mut chunk_end = position;
        let mut chunk_width = 0;
        while chunk_end < word_end {
            let item_width = width_of(chunk_end);
            if chunk_end > position && chunk_width + item_width > capacity {
                break;
            }
            chunk_width += item_width;
            chunk_end += 1;
            if chunk_width >= capacity {
                break;
            }
        }
        // Zero-width items consume no cells; keep them with this chunk.
        while chunk_end < word_end && width_of(chunk_end) == 0 {
            chunk_end += 1;
        }
        if chunk_end == position {
            // One atomic item wider than the capacity: emit it alone.
            chunk_end = position + 1;
            chunk_width = width_of(position);
        } else if chunk_width == 0 && chunk_end < word_end {
            // A chunk of only zero-width items would be invisible; attach the
            // next item so it cannot form a phantom row.
            chunk_width = width_of(chunk_end);
            chunk_end += 1;
        }
        if chunk_end < word_end {
            state.rows.push(WrapRow {
                start: position,
                end: chunk_end,
                indent,
            });
        } else {
            state.begin(position, indent);
            state.append(chunk_end, chunk_width);
        }
        position = chunk_end;
    }
}

/// Emits a whitespace run verbatim, wrapping onto unindented rows when it
/// overflows the current row.
fn emit_whitespace<T, F>(
    items: &[T],
    item: &F,
    ws_start: usize,
    ws_end: usize,
    width: usize,
    state: &mut WrapState,
) where
    F: Fn(&T) -> &str,
{
    let mut position = ws_start;
    while position < ws_end {
        let item_width = UnicodeWidthStr::width(item(&items[position]));
        let available = width.saturating_sub(state.row_indent);
        if state.row_end > state.row_start && state.used + item_width > available {
            state.flush();
            state.begin(position, 0);
            continue;
        }
        state.append(position + 1, item_width);
        position += 1;
    }
}

#[cfg(test)]
mod tests {
    use unicode_segmentation::UnicodeSegmentation;

    use super::{is_breakable_whitespace, wrap_items};

    fn rows(text: &str, width: usize) -> Vec<String> {
        let graphemes: Vec<&str> = text.graphemes(true).collect();
        wrap_items(
            &graphemes,
            width,
            |grapheme| *grapheme,
            |grapheme| is_breakable_whitespace(grapheme.chars().next().unwrap_or('\0')),
        )
        .into_iter()
        .map(|row| {
            let mut line = String::new();
            line.push_str(&" ".repeat(row.indent));
            for grapheme in &graphemes[row.start..row.end] {
                line.push_str(grapheme);
            }
            line
        })
        .collect()
    }

    #[test]
    fn keeps_whole_words_that_fit_on_the_next_row() {
        assert_eq!(rows("alpha beta", 7), ["alpha", "beta"]);
        assert_eq!(rows("a abcde", 5), ["a", "abcde"]);
    }

    #[test]
    fn splits_overwide_words_with_a_two_space_continuation_indent() {
        assert_eq!(rows("abcdefghij", 6), ["abcdef", "  ghij"]);
        assert_eq!(rows("abcdefghijklmn", 6), ["abcdef", "  ghij", "  klmn"]);
        assert_eq!(rows("go abcdefghij", 6), ["go", "abcdef", "  ghij"]);
        assert_eq!(rows("abcdefg abcde", 6), ["abcdef", "  g", "abcde"]);
        assert_eq!(rows("abcdefgh x", 6), ["abcdef", "  gh x"]);
    }

    #[test]
    fn preserves_internal_whitespace_until_a_wrap_point() {
        assert_eq!(rows("aa   bb", 7), ["aa   bb"]);
        assert_eq!(rows("aa   bb", 6), ["aa", "bb"]);
    }

    #[test]
    fn preserves_leading_and_trailing_whitespace() {
        assert_eq!(rows("  abc", 4), ["  ", "abc"]);
        assert_eq!(rows("abc  ", 4), ["abc ", " "]);
    }

    #[test]
    fn newline_characters_act_as_whitespace_separators() {
        // Callers split logical lines before wrapping; the core treats '\n'
        // as ordinary whitespace so it never creates rows by itself.
        assert_eq!(rows("ab\ncd", 2), ["ab", "cd"]);
    }

    #[test]
    fn wide_characters_are_atomic_and_wrap_by_cells() {
        assert_eq!(rows("ab界cd", 4), ["ab界", "  cd"]);
        assert_eq!(rows("界界界", 4), ["界界", "  界"]);
        assert_eq!(rows("x 界界", 4), ["x", "界界"]);
    }

    #[test]
    fn narrow_widths_clamp_the_indent_and_force_progress() {
        assert_eq!(rows("ab", 0), ["a", "b"]);
        assert_eq!(rows("ab", 1), ["a", "b"]);
        assert_eq!(rows("abc", 2), ["ab", " c"]);
        assert_eq!(rows("abcd", 2), ["ab", " c", " d"]);
        assert_eq!(rows("界", 1), ["界"]);
        // The continuation indent never widens a row past the container.
        assert_eq!(rows("界界", 2), ["界", "界"]);
        assert_eq!(rows("界ab", 2), ["界", " a", " b"]);
    }

    #[test]
    fn zero_width_characters_stay_with_their_chunk() {
        assert_eq!(rows("e\u{301}x", 10), ["e\u{301}x"]);
    }

    #[test]
    fn emoji_zwj_sequences_are_atomic() {
        assert_eq!(rows("👩‍💻x", 3), ["👩‍💻x"]);
        assert_eq!(rows("👩‍💻x", 2), ["👩‍💻", " x"]);
        assert_eq!(rows("👍🏽", 1), ["👍🏽"]);
    }

    #[test]
    fn non_breaking_spaces_are_not_break_points() {
        assert_eq!(rows("alpha\u{00A0}b", 7), ["alpha\u{00A0}b"]);
        assert_eq!(rows("alpha\u{00A0}b", 6), ["alpha\u{00A0}", "  b"]);
    }

    #[test]
    fn zero_width_space_is_an_explicit_break_opportunity() {
        assert_eq!(rows("foo\u{200B}bar", 3), ["foo", "bar"]);
    }
}
