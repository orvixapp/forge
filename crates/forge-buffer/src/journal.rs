//! Append-only crash journal (ARCHITECTURE.md §26): one file per unsaved
//! buffer, one JSON line per transaction. After a crash the file is
//! replayed on top of the last saved content.

use crate::transaction::Transaction;
use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Longest time a recorded transaction may sit in the OS cache before the
/// journal is fsynced.
pub const FSYNC_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    file: File,
    last_sync: Instant,
    pending_sync: bool,
}

impl Journal {
    /// Opens (creating) the journal at `path`.
    ///
    /// # Errors
    ///
    /// I/O errors creating the directory or the file.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file,
            last_sync: Instant::now(),
            pending_sync: false,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one transaction and fsyncs when the interval elapsed.
    ///
    /// # Errors
    ///
    /// Serialization or I/O failure; the buffer edit already happened.
    pub fn append(&mut self, transaction: &Transaction) -> io::Result<()> {
        let mut line = serde_json::to_vec(transaction)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.pending_sync = true;
        if self.last_sync.elapsed() >= FSYNC_INTERVAL {
            self.sync()?;
        }
        Ok(())
    }

    /// Forces pending writes to disk.
    ///
    /// # Errors
    ///
    /// The fsync failing.
    pub fn sync(&mut self) -> io::Result<()> {
        if self.pending_sync {
            self.file.sync_data()?;
            self.pending_sync = false;
        }
        self.last_sync = Instant::now();
        Ok(())
    }

    /// The buffer was saved: nothing left to recover.
    pub fn truncate(&mut self) {
        let _ = self.file.set_len(0);
        self.pending_sync = false;
    }

    /// Transactions recorded in `path`, in order. A truncated last line
    /// (crash mid-write) is dropped; anything else malformed is an error.
    ///
    /// # Errors
    ///
    /// I/O or JSON errors before the last line.
    pub fn read(path: &Path) -> io::Result<Vec<Transaction>> {
        let reader = BufReader::new(File::open(path)?);
        let lines: Vec<String> = reader.lines().collect::<io::Result<_>>()?;
        let mut transactions = Vec::with_capacity(lines.len());
        let last = lines.len().saturating_sub(1);
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Transaction>(line) {
                Ok(transaction) => transactions.push(transaction),
                Err(_) if index == last => break,
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
        Ok(transactions)
    }

    /// Where the journal of `file` lives inside `journal_dir`: a stable name
    /// derived from the absolute path.
    #[must_use]
    pub fn path_for(journal_dir: &Path, file: &Path) -> PathBuf {
        let mut name = String::new();
        for c in file.to_string_lossy().chars() {
            name.push(if c.is_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            });
        }
        if name.len() > 120 {
            let hash = fnv1a(file.to_string_lossy().as_bytes());
            name = format!("{}-{hash:016x}", &name[name.len() - 100..]);
        }
        journal_dir.join(format!("{name}.journal"))
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Buffer, Edit};

    #[test]
    fn a_journal_replays_unsaved_edits_and_drops_a_torn_tail() {
        let dir = std::env::temp_dir().join(format!("forge-journal-{}", std::process::id()));
        let path = Journal::path_for(&dir, Path::new("/tmp/some file.rs"));
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("some_file.rs.journal")
        );
        let mut buffer = Buffer::new("base\n");
        buffer.attach_journal(Journal::open(&path).unwrap());
        buffer.edit(vec![Edit::insert(5, "one\n")], false).unwrap();
        buffer.edit(vec![Edit::insert(9, "two\n")], false).unwrap();
        buffer.journal().unwrap();
        // Simulate a crash mid-write of a third line.
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"{\"edits\":[{\"range\":{\"start\":0,\"end\":")
                .unwrap();
        }
        let recorded = Journal::read(&path).unwrap();
        assert_eq!(recorded.len(), 2);
        let mut recovered = Buffer::new("base\n");
        for transaction in &recorded {
            recovered.replay(transaction).unwrap();
        }
        assert_eq!(recovered.text(), "base\none\ntwo\n");
        buffer.mark_saved();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "saving empties the journal"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
