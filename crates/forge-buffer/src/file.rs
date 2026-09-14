//! Loading and saving files: encoding detection (BOM, UTF-8, fallback to
//! Windows-1252 through `encoding_rs`), line-ending normalisation and
//! atomic writes.

use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

/// A file decoded into text plus what is needed to write it back the same
/// way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedFile {
    pub path: PathBuf,
    /// Text with `\n` line endings only.
    pub text: String,
    pub line_ending: LineEnding,
    /// `encoding_rs` name, e.g. `UTF-8`, `UTF-16LE`, `windows-1252`.
    pub encoding: &'static str,
    pub had_bom: bool,
    /// Bytes could not be decoded losslessly (replacement characters).
    pub lossy: bool,
}

impl LoadedFile {
    /// Reads and decodes `path`.
    ///
    /// # Errors
    ///
    /// I/O errors; undecodable content is never an error (it is flagged).
    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = std::fs::read(&path)?;
        Ok(Self::decode(path, &bytes))
    }

    /// Decodes bytes read from `path`.
    #[must_use]
    pub fn decode(path: PathBuf, bytes: &[u8]) -> Self {
        let (encoding, bom_len) = encoding_rs::Encoding::for_bom(bytes)
            .unwrap_or((encoding_rs::UTF_8, 0));
        let body = &bytes[bom_len..];
        let (text, lossy) = if encoding == encoding_rs::UTF_8 {
            match std::str::from_utf8(body) {
                Ok(text) => (text.to_owned(), false),
                // Not UTF-8: the common Windows legacy encoding is the best
                // guess; the user can re-open with another one later.
                Err(_) => {
                    let (text, _, _) = encoding_rs::WINDOWS_1252.decode(body);
                    return Self {
                        path,
                        line_ending: detect_line_ending(&text),
                        text: normalize(&text),
                        encoding: encoding_rs::WINDOWS_1252.name(),
                        had_bom: false,
                        lossy: false,
                    };
                }
            }
        } else {
            let (text, _, had_errors) = encoding.decode_without_bom_handling(body);
            (text.into_owned(), had_errors)
        };
        Self {
            path,
            line_ending: detect_line_ending(&text),
            text: normalize(&text),
            encoding: encoding.name(),
            had_bom: bom_len > 0,
            lossy,
        }
    }

    /// Encodes `text` (with `\n` endings) the way the file was read and
    /// writes it atomically (temporary file + rename).
    ///
    /// # Errors
    ///
    /// I/O errors; the original file is untouched on failure.
    pub fn write(&self, text: &str) -> io::Result<()> {
        let bytes = self.encode(text);
        write_atomic(&self.path, &bytes)
    }

    #[must_use]
    pub fn encode(&self, text: &str) -> Vec<u8> {
        let text = if self.line_ending == LineEnding::CrLf {
            text.replace('\n', "\r\n")
        } else {
            text.to_owned()
        };
        let encoding = encoding_rs::Encoding::for_label(self.encoding.as_bytes())
            .unwrap_or(encoding_rs::UTF_8);
        let mut out = Vec::with_capacity(text.len() + 3);
        if self.had_bom {
            match encoding.name() {
                "UTF-8" => out.extend_from_slice(&[0xef, 0xbb, 0xbf]),
                "UTF-16LE" => out.extend_from_slice(&[0xff, 0xfe]),
                "UTF-16BE" => out.extend_from_slice(&[0xfe, 0xff]),
                _ => {}
            }
        }
        if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
            for unit in text.encode_utf16() {
                let bytes = if encoding == encoding_rs::UTF_16LE {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                out.extend_from_slice(&bytes);
            }
        } else {
            let (encoded, _, _) = encoding.encode(&text);
            out.extend_from_slice(&encoded);
        }
        out
    }
}

fn detect_line_ending(text: &str) -> LineEnding {
    if text.contains("\r\n") {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    }
}

fn normalize(text: &str) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_owned()
    }
}

/// Writes through a sibling temporary file and renames over `path`, so a
/// crash never leaves a half-written file.
///
/// # Errors
///
/// I/O errors.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let temporary = parent.join(format!(".{name}.forge-tmp"));
    std::fs::write(&temporary, bytes)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temporary, metadata.permissions());
    }
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_crlf_files_round_trip() {
        let bytes = b"\xef\xbb\xbfhola\r\nmundo\r\n";
        let file = LoadedFile::decode(PathBuf::from("x.txt"), bytes);
        assert_eq!(file.text, "hola\nmundo\n");
        assert_eq!(file.line_ending, LineEnding::CrLf);
        assert!(file.had_bom && !file.lossy);
        assert_eq!(file.encode(&file.text), bytes);
    }

    #[test]
    fn latin1_and_utf16_are_decoded() {
        let latin = LoadedFile::decode(PathBuf::from("l.txt"), b"a\xf1o\n");
        assert_eq!(latin.text, "año\n");
        assert_eq!(latin.encoding, "windows-1252");
        assert_eq!(latin.encode("año\n"), b"a\xf1o\n");
        let utf16 = LoadedFile::decode(PathBuf::from("u.txt"), b"\xff\xfeh\x00i\x00");
        assert_eq!(utf16.text, "hi");
        assert_eq!(utf16.encoding, "UTF-16LE");
        assert_eq!(utf16.encode("hi"), b"\xff\xfeh\x00i\x00");
    }

    #[test]
    fn atomic_write_replaces_the_file() {
        let dir = std::env::temp_dir().join(format!("forge-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "old").unwrap();
        let file = LoadedFile::read(&path).unwrap();
        file.write("new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(!dir.join(".a.txt.forge-tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
