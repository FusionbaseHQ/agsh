//! A single-line edit buffer over Unicode scalar values, with cursor motions
//! and a kill-ring. Pure and unit-testable; the editor wires it to the terminal.

#[derive(Debug, Default, Clone)]
pub struct LineBuffer {
    chars: Vec<char>,
    cursor: usize,
    kill: String,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    /// Replace the whole buffer and place the cursor at the end.
    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn insert_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Replace the char range `start..end` with `text`, leaving the cursor after
    /// the inserted text. Used to accept a completion.
    pub fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        self.chars.splice(start..end, text.chars());
        self.cursor = start + text.chars().count();
    }

    /// The substring of char range `start..end` (clamped).
    pub fn text_range(&self, start: usize, end: usize) -> String {
        let start = start.min(self.chars.len());
        let end = end.min(self.chars.len()).max(start);
        self.chars[start..end].iter().collect()
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    pub fn at_end(&self) -> bool {
        self.cursor == self.chars.len()
    }

    pub fn word_left(&mut self) {
        self.cursor = self.prev_word_start();
    }

    pub fn word_right(&mut self) {
        self.cursor = self.next_word_end();
    }

    /// Kill from the cursor to the end of the line into the kill-ring.
    pub fn kill_to_end(&mut self) {
        if self.cursor < self.chars.len() {
            self.kill = self.chars[self.cursor..].iter().collect();
            self.chars.truncate(self.cursor);
        }
    }

    /// Kill from the start of the line to the cursor into the kill-ring.
    pub fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.kill = self.chars[..self.cursor].iter().collect();
            self.chars.drain(0..self.cursor);
            self.cursor = 0;
        }
    }

    /// Kill the word before the cursor into the kill-ring.
    pub fn kill_word(&mut self) {
        let start = self.prev_word_start();
        if start < self.cursor {
            self.kill = self.chars[start..self.cursor].iter().collect();
            self.chars.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    /// Delete the word after the cursor.
    pub fn delete_word(&mut self) {
        let end = self.next_word_end();
        if end > self.cursor {
            self.chars.drain(self.cursor..end);
        }
    }

    /// Insert the kill-ring contents at the cursor.
    pub fn yank(&mut self) {
        let kill = std::mem::take(&mut self.kill);
        self.insert_str(&kill);
        self.kill = kill;
    }

    fn prev_word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn next_word_end(&self) -> usize {
        let n = self.chars.len();
        let mut i = self.cursor;
        while i < n && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && !self.chars[i].is_whitespace() {
            i += 1;
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_move() {
        let mut b = LineBuffer::new();
        b.insert_str("echo hi");
        assert_eq!(b.text(), "echo hi");
        assert_eq!(b.cursor(), 7);
        b.home();
        assert_eq!(b.cursor(), 0);
        b.word_right();
        assert_eq!(b.cursor(), 4); // after "echo"
        b.end();
        b.backspace();
        assert_eq!(b.text(), "echo h");
    }

    #[test]
    fn kill_and_yank() {
        let mut b = LineBuffer::new();
        b.set("git commit");
        b.kill_word();
        assert_eq!(b.text(), "git ");
        b.yank();
        assert_eq!(b.text(), "git commit");
    }

    #[test]
    fn kill_to_end_and_start() {
        let mut b = LineBuffer::new();
        b.set("hello world");
        b.home();
        b.word_right();
        b.kill_to_end();
        assert_eq!(b.text(), "hello");
        b.end();
        b.kill_to_start();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn word_motions_over_spaces() {
        let mut b = LineBuffer::new();
        b.set("  ab  cd  ");
        b.home();
        b.word_right();
        assert_eq!(b.cursor(), 4); // after "ab"
        b.word_right();
        assert_eq!(b.cursor(), 8); // after "cd"
        b.word_left();
        assert_eq!(b.cursor(), 6); // start of "cd"
    }
}
