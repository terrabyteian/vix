use crate::buffer::Buffer;

/// A single cursor. `head` is where the cursor logically sits.
/// `anchor` is where the visual selection started (in Visual mode).
/// Outside Visual mode, anchor == head.
/// Positions are char offsets into the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Virtual column: target column for vertical motions.
    /// Preserved across j/k so moving through short lines doesn't lose your column.
    pub virt_col: Option<usize>,
}

impl Selection {
    pub fn at(pos: usize) -> Self {
        Self {
            anchor: pos,
            head: pos,
            virt_col: None,
        }
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        if self.anchor <= self.head {
            self.anchor..self.head
        } else {
            self.head..self.anchor
        }
    }

    /// Inclusive range ending at head's position — used for Vim-style visual selection
    /// where the character under the cursor is included.
    pub fn inclusive_range(&self, buf: &Buffer) -> std::ops::Range<usize> {
        let r = self.range();
        let end = (r.end + 1).min(buf.len_chars());
        r.start..end
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Clamp head into [0, len_chars]. Returns self for chaining.
    pub fn clamped(mut self, buf: &Buffer) -> Self {
        let max = buf.len_chars();
        self.anchor = self.anchor.min(max);
        self.head = self.head.min(max);
        self
    }

    /// Move head to `pos`, keeping anchor.
    pub fn extend_to(mut self, pos: usize) -> Self {
        self.head = pos;
        self
    }

    /// Move both anchor and head to `pos`.
    pub fn move_to(self, pos: usize) -> Self {
        Self {
            anchor: pos,
            head: pos,
            virt_col: self.virt_col,
        }
    }

    pub fn with_virt_col(mut self, col: Option<usize>) -> Self {
        self.virt_col = col;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_is_normalized() {
        let s = Selection {
            anchor: 5,
            head: 2,
            virt_col: None,
        };
        assert_eq!(s.range(), 2..5);
        let s = Selection {
            anchor: 2,
            head: 5,
            virt_col: None,
        };
        assert_eq!(s.range(), 2..5);
    }

    #[test]
    fn clamp_to_buffer() {
        let buf = Buffer::from_text("hello");
        let s = Selection::at(100).clamped(&buf);
        assert_eq!(s.head, 5);
    }
}
