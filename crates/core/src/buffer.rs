use ropey::Rope;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("buffer has no associated file path")]
    NoPath,
}

pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    dirty: bool,
    /// Monotonic counter bumped on every structural mutation. Downstream
    /// caches (syntax spans, etc.) compare against this to know when to
    /// rebuild.
    version: u64,
}

impl Buffer {
    pub fn empty() -> Self {
        Self { rope: Rope::new(), path: None, dirty: false, version: 0 }
    }

    pub fn from_text(s: &str) -> Self {
        Self { rope: Rope::from_str(s), path: None, dirty: false, version: 0 }
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, BufferError> {
        // Absolutize: downstream (LSP URIs, buffer dedup, save) assumes absolute
        // paths. Join with cwd rather than canonicalizing so this works for
        // files that don't exist yet (e.g. `:e new_file.rs`).
        let raw = path.as_ref();
        let p = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(raw))
                .unwrap_or_else(|_| raw.to_path_buf())
        };
        let rope = if p.exists() {
            Rope::from_reader(BufReader::new(File::open(&p)?))?
        } else {
            Rope::new()
        };
        Ok(Self { rope, path: Some(p), dirty: false, version: 0 })
    }

    pub fn save(&mut self) -> Result<(), BufferError> {
        let path = self.path.as_ref().ok_or(BufferError::NoPath)?.clone();
        self.save_as(&path)
    }

    pub fn save_as<P: AsRef<Path>>(&mut self, path: P) -> Result<(), BufferError> {
        let p = path.as_ref().to_path_buf();
        let file = File::create(&p)?;
        self.rope.write_to(BufWriter::new(file))?;
        self.path = Some(p);
        self.dirty = false;
        Ok(())
    }

    pub fn rope(&self) -> &Rope { &self.rope }
    pub fn rope_mut(&mut self) -> &mut Rope {
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
        &mut self.rope
    }

    pub fn path(&self) -> Option<&Path> { self.path.as_deref() }
    pub fn set_path<P: AsRef<Path>>(&mut self, p: P) { self.path = Some(p.as_ref().to_path_buf()); }
    pub fn dirty(&self) -> bool { self.dirty }
    pub fn version(&self) -> u64 { self.version }

    pub fn len_chars(&self) -> usize { self.rope.len_chars() }
    pub fn len_lines(&self) -> usize { self.rope.len_lines() }

    /// Return (line, column) for a char offset. Column is char-based (0-indexed).
    pub fn char_to_line_col(&self, ch: usize) -> (usize, usize) {
        let line = self.rope.char_to_line(ch);
        let line_start = self.rope.line_to_char(line);
        (line, ch - line_start)
    }

    /// Char offset of the start of a line. Clamps to buffer length.
    pub fn line_to_char(&self, line: usize) -> usize {
        let line = line.min(self.len_lines().saturating_sub(1));
        self.rope.line_to_char(line)
    }

    /// Length of a line in chars, excluding the trailing newline.
    pub fn line_len_chars(&self, line: usize) -> usize {
        if line >= self.len_lines() {
            return 0;
        }
        let slice = self.rope.line(line);
        let len = slice.len_chars();
        // Strip trailing \n if present (ropey includes it in line slices).
        if len > 0 && slice.char(len - 1) == '\n' { len - 1 } else { len }
    }

    pub fn insert_char(&mut self, ch: usize, c: char) {
        self.rope.insert_char(ch, c);
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
    }

    pub fn insert_str(&mut self, ch: usize, s: &str) {
        self.rope.insert(ch, s);
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
    }

    pub fn remove_range(&mut self, range: std::ops::Range<usize>) {
        self.rope.remove(range);
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer() {
        let b = Buffer::empty();
        assert_eq!(b.len_chars(), 0);
        assert_eq!(b.len_lines(), 1);
        assert!(!b.dirty());
    }

    #[test]
    fn from_str_and_line_ops() {
        let b = Buffer::from_text("hello\nworld\n");
        assert_eq!(b.len_lines(), 3); // ropey counts trailing newline as a new empty line
        assert_eq!(b.line_len_chars(0), 5);
        assert_eq!(b.line_len_chars(1), 5);
        assert_eq!(b.line_len_chars(2), 0);
    }

    #[test]
    fn char_to_line_col_roundtrip() {
        let b = Buffer::from_text("ab\ncd\nef");
        assert_eq!(b.char_to_line_col(0), (0, 0));
        assert_eq!(b.char_to_line_col(2), (0, 2));
        assert_eq!(b.char_to_line_col(3), (1, 0));
        assert_eq!(b.char_to_line_col(6), (2, 0));
    }

    #[test]
    fn insert_marks_dirty() {
        let mut b = Buffer::from_text("abc");
        assert!(!b.dirty());
        b.insert_char(1, 'X');
        assert!(b.dirty());
        assert_eq!(b.rope().to_string(), "aXbc");
    }

    #[test]
    fn version_increments_on_mutation() {
        let mut b = Buffer::from_text("abc");
        let v0 = b.version();
        b.insert_char(0, 'X');
        assert_ne!(b.version(), v0);
        let v1 = b.version();
        b.insert_str(0, "Y");
        assert_ne!(b.version(), v1);
        let v2 = b.version();
        b.remove_range(0..1);
        assert_ne!(b.version(), v2);
    }

    #[test]
    fn version_stable_on_reads() {
        let b = Buffer::from_text("abc");
        let v0 = b.version();
        let _ = b.rope();
        let _ = b.len_chars();
        let _ = b.char_to_line_col(1);
        assert_eq!(b.version(), v0);
    }
}
