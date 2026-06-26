//! A small, reusable full-screen multi-line text editor (nano-style).
//!
//! [`TextEditorState`] is a self-contained 2D text buffer with a char-indexed
//! cursor. It is mode-agnostic: it owns no rendering and no key mapping, only
//! the buffer + cursor mutations. The `/agents` prompt editor drives it (open on
//! the Prompt field, edit comfortably, Esc commits the text back into the
//! draft), but nothing here is agents-specific.
//!
//! # Invariants
//! - `lines` is never empty: an empty buffer is `vec![String::new()]`, so `row`
//!   always indexes a real line.
//! - `row` is in `0..lines.len()`; `col` is a CHAR index in
//!   `0..=lines[row].chars().count()` (one past the last char = end-of-line).
//! - All edits / moves end by calling [`clamp`](TextEditorState::clamp), so the
//!   two bounds above hold after every public mutation.
//!
//! # Unicode
//! Every operation works on `char`s (via `chars().count()` and char-index
//! splicing), never raw byte offsets, so multi-byte text never splits a
//! codepoint or panics on a byte boundary.

/// A 2D text buffer with a char-indexed cursor and vertical scroll offset.
///
/// See the module docs for the invariants the methods preserve.
#[derive(Debug, Clone)]
pub struct TextEditorState {
    /// The text, one entry per logical line (no trailing `\n`). Never empty.
    pub lines: Vec<String>,
    /// Cursor line index, in `0..lines.len()`.
    pub row: usize,
    /// Cursor column as a CHAR index, in `0..=lines[row].chars().count()`.
    pub col: usize,
    /// Index of the top visible line (the view scrolls vertically by whole
    /// lines). The renderer adjusts this to keep the cursor row on screen.
    pub scroll: usize,
}

impl TextEditorState {
    /// Build an editor seeded with `s`, splitting on `'\n'` into lines.
    ///
    /// An empty `s` yields a single empty line (the buffer is never empty). The
    /// cursor starts at the very top-left and the view is scrolled to the top.
    pub fn from_text(s: &str) -> Self {
        let lines: Vec<String> = if s.is_empty() {
            vec![String::new()]
        } else {
            s.split('\n').map(|l| l.to_string()).collect()
        };
        Self {
            lines,
            row: 0,
            col: 0,
            scroll: 0,
        }
    }

    /// The full buffer as a single string, lines re-joined with `'\n'`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Number of chars on the current cursor line.
    fn line_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    /// Insert `c` at the cursor by CHAR index, then advance the cursor past it.
    pub fn insert_char(&mut self, c: char) {
        let mut chars: Vec<char> = self.lines[self.row].chars().collect();
        let at = self.col.min(chars.len());
        chars.insert(at, c);
        self.lines[self.row] = chars.into_iter().collect();
        self.col = at + 1;
        self.clamp();
    }

    /// Insert a (possibly multi-line) string at the cursor (paste path).
    ///
    /// The current line is split at the cursor; `s` is spliced in between the
    /// head and the tail, honouring its own `'\n'`s. The cursor ends at the end
    /// of the inserted text:
    /// - single-line `s` → same row, `col` advanced by `s.chars().count()`;
    /// - multi-line `s` → the last inserted line, `col` = (len of `s`'s last
    ///   segment), with the original tail appended after it.
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        // Split the current line at the cursor into head | tail (char-indexed).
        let cur: Vec<char> = self.lines[self.row].chars().collect();
        let at = self.col.min(cur.len());
        let head: String = cur[..at].iter().collect();
        let tail: String = cur[at..].iter().collect();

        let segments: Vec<&str> = s.split('\n').collect();
        if segments.len() == 1 {
            // No newline in the paste: stitch head + s + tail back onto one line.
            let inserted_len = segments[0].chars().count();
            self.lines[self.row] = format!("{head}{}{tail}", segments[0]);
            self.col = at + inserted_len;
        } else {
            // Multi-line: first segment joins the head; middle segments become
            // whole new lines; the last segment carries the original tail.
            let last_idx = segments.len() - 1;
            let last_len = segments[last_idx].chars().count();

            let mut new_block: Vec<String> = Vec::with_capacity(segments.len());
            new_block.push(format!("{head}{}", segments[0]));
            for seg in &segments[1..last_idx] {
                new_block.push((*seg).to_string());
            }
            new_block.push(format!("{}{tail}", segments[last_idx]));

            let new_row = self.row + last_idx;
            // Replace the current line with the whole expanded block.
            self.lines.splice(self.row..=self.row, new_block);
            self.row = new_row;
            self.col = last_len;
        }
        self.clamp();
    }

    /// Split the current line at the cursor; the tail drops to a fresh line
    /// below and the cursor moves to its start (column 0).
    pub fn newline(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let at = self.col.min(chars.len());
        let head: String = chars[..at].iter().collect();
        let tail: String = chars[at..].iter().collect();
        self.lines[self.row] = head;
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
        self.clamp();
    }

    /// Delete the char before the cursor.
    ///
    /// Mid-line (`col > 0`) removes one char and steps left. At column 0 of a
    /// non-first line it joins this line onto the end of the previous one, with
    /// the cursor landing at the join point (the previous line's old length).
    pub fn backspace(&mut self) {
        if self.col > 0 {
            let mut chars: Vec<char> = self.lines[self.row].chars().collect();
            self.col -= 1;
            chars.remove(self.col);
            self.lines[self.row] = chars.into_iter().collect();
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            let prev_len = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&cur);
            self.col = prev_len;
        }
        self.clamp();
    }

    /// Delete the char at the cursor (forward delete).
    ///
    /// Mid-line removes the char under the cursor (cursor stays put). At the end
    /// of a non-last line it joins the next line onto this one (cursor stays at
    /// the join point).
    pub fn delete(&mut self) {
        let len = self.line_len();
        if self.col < len {
            let mut chars: Vec<char> = self.lines[self.row].chars().collect();
            chars.remove(self.col);
            self.lines[self.row] = chars.into_iter().collect();
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.clamp();
    }

    /// Move the cursor one char left; past column 0 it wraps to the end of the
    /// previous line.
    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len();
        }
        self.clamp();
    }

    /// Move the cursor one char right; past end-of-line it wraps to the start of
    /// the next line.
    pub fn move_right(&mut self) {
        if self.col < self.line_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.clamp();
    }

    /// Move the cursor up one line, clamping the column to the new line's length.
    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.line_len());
        }
        self.clamp();
    }

    /// Move the cursor down one line, clamping the column to the new line's length.
    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.line_len());
        }
        self.clamp();
    }

    /// Move the cursor to the start of the current line.
    pub fn home(&mut self) {
        self.col = 0;
    }

    /// Move the cursor to the end of the current line.
    pub fn end(&mut self) {
        self.col = self.line_len();
    }

    /// Re-establish the invariants: keep `row` in range and `col` no further than
    /// the end of the current line. Called at the end of every mutation/move.
    pub fn clamp(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        if self.row >= self.lines.len() {
            self.row = self.lines.len() - 1;
        }
        let len = self.line_len();
        if self.col > len {
            self.col = len;
        }
    }
}
