//! Bounded text accumulator with whitespace collapsing.

/// Collects extracted text up to a character budget.
///
/// [`Sink::text`] collapses every run of whitespace to one space and drops
/// control characters; [`Sink::newline`] ends a paragraph. Leading and
/// trailing spaces of a line are never emitted, and empty lines never occur.
#[derive(Debug)]
pub(crate) struct Sink {
    buf: String,
    chars: usize,
    max: usize,
    truncated: bool,
    pending_space: bool,
}

impl Sink {
    pub(crate) fn new(max_chars: usize) -> Self {
        Sink {
            buf: String::new(),
            chars: 0,
            max: max_chars,
            truncated: false,
            pending_space: false,
        }
    }

    /// Whether the budget is spent; callers may stop parsing.
    pub(crate) fn is_full(&self) -> bool {
        self.truncated
    }

    /// Appends `s`, collapsing whitespace.
    pub(crate) fn text(&mut self, s: &str) {
        for c in s.chars() {
            if self.truncated {
                return;
            }
            if c.is_whitespace() {
                self.pending_space = true;
                continue;
            }
            if c.is_control() || c == '\u{FEFF}' {
                continue;
            }
            let at_line_start = self.buf.is_empty() || self.buf.ends_with('\n');
            let space = self.pending_space && !at_line_start;
            let needed = 1 + usize::from(space);
            if self.chars + needed > self.max {
                self.truncated = true;
                return;
            }
            if space {
                self.buf.push(' ');
            }
            self.buf.push(c);
            self.chars += needed;
            self.pending_space = false;
        }
    }

    /// Separates the next text from the previous one by a space.
    pub(crate) fn space(&mut self) {
        self.pending_space = true;
    }

    /// Ends the current paragraph.
    pub(crate) fn newline(&mut self) {
        self.pending_space = false;
        if self.truncated || self.buf.is_empty() || self.buf.ends_with('\n') {
            return;
        }
        if self.chars < self.max {
            self.buf.push('\n');
            self.chars += 1;
        }
    }

    /// Appends text that keeps its own line structure: each line goes
    /// through [`Sink::text`], line breaks become paragraph ends.
    pub(crate) fn lines(&mut self, s: &str) {
        for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            self.text(line);
            if self.truncated {
                return;
            }
        }
    }

    /// Returns the text and whether it was truncated.
    pub(crate) fn finish(mut self) -> (String, bool) {
        while self.buf.ends_with('\n') {
            self.buf.pop();
        }
        (self.buf, self.truncated)
    }
}
