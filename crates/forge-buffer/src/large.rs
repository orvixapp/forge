//! Large-file mode (ARCHITECTURE.md §13.4): files above the threshold are
//! memory-mapped and read through a line index built in the background,
//! never loaded into a rope until the user edits them.

// The only panics are poisoned-mutex expectations on the line index.
#![allow(clippy::missing_panics_doc)]

use memmap2::Mmap;
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

/// Files at or above this size open in large mode.
pub const LARGE_FILE_BYTES: u64 = 200 * 1024 * 1024;
/// A single line this long also forces large mode (minified JSON).
pub const LARGE_LINE_BYTES: usize = 1024 * 1024;
/// Rows scanned between index publications.
const INDEX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Byte offsets of line starts, filled in by the indexing thread.
#[derive(Debug, Default)]
struct LineIndex {
    /// `starts[i]` is the byte offset of line `i`; `starts[0] == 0`.
    starts: Vec<usize>,
    complete: bool,
}

/// A memory-mapped, read-only view of a big file.
pub struct LargeFile {
    path: PathBuf,
    map: Arc<Mmap>,
    index: Arc<Mutex<LineIndex>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for LargeFile {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl LargeFile {
    /// Maps `path` and starts indexing its lines on a thread.
    ///
    /// # Errors
    ///
    /// I/O errors opening or mapping the file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: the mapping is read-only; concurrent writers to the file
        // would change bytes under us, which is the documented trade-off
        // of large mode (the watcher reports the change).
        let map = unsafe { Mmap::map(&file)? };
        let map = Arc::new(map);
        let index = Arc::new(Mutex::new(LineIndex {
            starts: vec![0],
            complete: false,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let map = Arc::clone(&map);
            let index = Arc::clone(&index);
            let cancel = Arc::clone(&cancel);
            thread::Builder::new()
                .name("forge-line-index".into())
                .spawn(move || build_index(&map, &index, &cancel))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            map,
            index,
            cancel,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.map.len()
    }

    /// Lines indexed so far; equals the real count once `is_indexed`.
    #[must_use]
    pub fn len_lines(&self) -> usize {
        let index = self.index.lock().expect("line index poisoned");
        if index.complete {
            index.starts.len()
        } else {
            index.starts.len().saturating_sub(1).max(1)
        }
    }

    #[must_use]
    pub fn is_indexed(&self) -> bool {
        self.index.lock().expect("line index poisoned").complete
    }

    /// Text of line `line` (without terminator), lossily decoded and
    /// truncated to `max_bytes` so a pathological line cannot stall the
    /// renderer. `None` past the indexed lines.
    #[must_use]
    pub fn line(&self, line: usize, max_bytes: usize) -> Option<String> {
        let (start, end) = {
            let index = self.index.lock().expect("line index poisoned");
            let start = *index.starts.get(line)?;
            let end = match index.starts.get(line + 1) {
                Some(next) => next - 1,
                None if index.complete => self.map.len(),
                None => return None,
            };
            (start, end)
        };
        let end = end.min(start + max_bytes);
        let bytes = &self.map[start..end];
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Whole content as text, for materialising into a rope on first edit.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.map).into_owned()
    }
}

fn build_index(map: &Mmap, index: &Mutex<LineIndex>, cancel: &AtomicBool) {
    let mut pending: Vec<usize> = Vec::new();
    let mut offset = 0;
    while offset < map.len() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let end = (offset + INDEX_CHUNK_BYTES).min(map.len());
        for (position, byte) in map[offset..end].iter().enumerate() {
            if *byte == b'\n' {
                pending.push(offset + position + 1);
            }
        }
        offset = end;
        let mut guard = index.lock().expect("line index poisoned");
        guard.starts.append(&mut pending);
    }
    let mut guard = index.lock().expect("line index poisoned");
    // A trailing newline does not start another line.
    if guard.starts.len() > 1 && guard.starts.last() == Some(&map.len()) {
        guard.starts.pop();
    }
    guard.complete = true;
}

/// Whether `path` should open in large mode: by size, or by containing a
/// line longer than [`LARGE_LINE_BYTES`] within the first megabytes.
///
/// # Errors
///
/// Metadata or read errors.
pub fn is_large(path: &Path) -> io::Result<bool> {
    let size = std::fs::metadata(path)?.len();
    if size >= LARGE_FILE_BYTES {
        return Ok(true);
    }
    if size < LARGE_LINE_BYTES as u64 {
        return Ok(false);
    }
    // Look for a newline in each of the first few megabytes; a stretch
    // without one means a giant line.
    let file = File::open(path)?;
    // SAFETY: read-only mapping, see `LargeFile::open`.
    let map = unsafe { Mmap::map(&file)? };
    let probe = &map[..map.len().min(8 * LARGE_LINE_BYTES)];
    let mut last_newline = 0;
    for (position, byte) in probe.iter().enumerate() {
        if *byte == b'\n' {
            last_newline = position;
        } else if position - last_newline > LARGE_LINE_BYTES {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fmt::Write as _,
        time::{Duration, Instant},
    };

    #[test]
    fn indexes_lines_in_the_background_and_reads_them_by_number() {
        let dir = std::env::temp_dir().join(format!("forge-large-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.log");
        let mut content = String::new();
        for line in 0..50_000 {
            let _ = write!(content, "line {line} with some padding text\r\n");
        }
        std::fs::write(&path, &content).unwrap();
        let file = LargeFile::open(&path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !file.is_indexed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(file.is_indexed());
        assert_eq!(file.len_lines(), 50_000);
        assert_eq!(
            file.line(0, 1024).as_deref(),
            Some("line 0 with some padding text")
        );
        assert_eq!(
            file.line(49_999, 1024).as_deref(),
            Some("line 49999 with some padding text")
        );
        assert_eq!(file.line(50_000, 1024), None);
        assert_eq!(file.line(1, 6).as_deref(), Some("line 1"), "truncated");
        assert!(!is_large(&path).unwrap());
        let wide = dir.join("wide.json");
        std::fs::write(&wide, "x".repeat(2 * LARGE_LINE_BYTES)).unwrap();
        assert!(is_large(&wide).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
