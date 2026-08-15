//! RFC 4180 CSV reader/writer with streaming record iteration.
//!
//! This is hand-written rather than the `csv` crate for one reason: the dataset
//! format distinguishes an empty *unquoted* cell (NULL) from `""` (the empty
//! string), and no CSV crate reports whether a field was quoted — `csv` decodes
//! both to the same empty string. Reading NULLs correctly requires the flag, so
//! the codec stays in-repo.
//!
//! Quoted fields may contain commas, quotes (doubled), and newlines. Reader
//! accepts both \n and \r\n line endings; writer always emits \n and quotes
//! only when needed. An empty unquoted field is distinguished from a quoted
//! empty field ("") by the caller via `Field::quoted` — the dataset layer uses
//! that to tell NULL from empty string.

use std::io::{BufRead, Write};

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub text: String,
    pub quoted: bool,
}

pub struct CsvReader<R: BufRead> {
    inner: R,
    buf: Vec<u8>,
    /// Byte offset scanned so far within `buf`.
    pos: usize,
    eof: bool,
}

impl<R: BufRead> CsvReader<R> {
    pub fn new(inner: R) -> Self {
        CsvReader { inner, buf: Vec::new(), pos: 0, eof: false }
    }

    fn fill(&mut self) -> std::io::Result<bool> {
        // Pull one more chunk into buf; returns false at EOF.
        let chunk = self.inner.fill_buf()?;
        if chunk.is_empty() {
            self.eof = true;
            return Ok(false);
        }
        let n = chunk.len();
        self.buf.extend_from_slice(chunk);
        self.inner.consume(n);
        Ok(true)
    }

    fn byte_at(&mut self, i: usize) -> std::io::Result<Option<u8>> {
        while i >= self.buf.len() {
            if !self.fill()? {
                return Ok(None);
            }
        }
        Ok(Some(self.buf[i]))
    }

    /// Read the next record. Returns None at EOF. A trailing newline at EOF
    /// does not produce an empty final record.
    pub fn next_record(&mut self) -> std::io::Result<Option<Vec<Field>>> {
        // Skip a leading position at EOF.
        if self.byte_at(self.pos)?.is_none() {
            return Ok(None);
        }
        let mut fields = Vec::new();
        let mut text = String::new();
        let mut raw = Vec::new();
        let mut quoted = false;
        let mut in_quotes = false;
        let mut i = self.pos;
        loop {
            let b = self.byte_at(i)?;
            match b {
                None => {
                    flush_field(&mut raw, &mut text, &mut fields, quoted)?;
                    self.pos = i;
                    if in_quotes {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "unterminated quoted CSV field",
                        ));
                    }
                    self.compact(i);
                    return Ok(Some(fields));
                }
                Some(b'"') if in_quotes => {
                    if self.byte_at(i + 1)? == Some(b'"') {
                        raw.push(b'"');
                        i += 2;
                    } else {
                        in_quotes = false;
                        i += 1;
                    }
                }
                Some(b'"') if raw.is_empty() && !quoted => {
                    quoted = true;
                    in_quotes = true;
                    i += 1;
                }
                Some(b',') if !in_quotes => {
                    flush_field(&mut raw, &mut text, &mut fields, quoted)?;
                    quoted = false;
                    i += 1;
                }
                Some(b'\n') if !in_quotes => {
                    flush_field(&mut raw, &mut text, &mut fields, quoted)?;
                    self.pos = i + 1;
                    self.compact(i + 1);
                    return Ok(Some(fields));
                }
                Some(b'\r') if !in_quotes && self.byte_at(i + 1)? == Some(b'\n') => {
                    flush_field(&mut raw, &mut text, &mut fields, quoted)?;
                    self.pos = i + 2;
                    self.compact(i + 2);
                    return Ok(Some(fields));
                }
                Some(b) => {
                    raw.push(b);
                    i += 1;
                }
            }
        }
    }

    fn compact(&mut self, upto: usize) {
        self.buf.drain(..upto.min(self.buf.len()));
        self.pos = 0;
    }
}

fn flush_field(
    raw: &mut Vec<u8>,
    _text: &mut String,
    fields: &mut Vec<Field>,
    quoted: bool,
) -> std::io::Result<()> {
    let s = String::from_utf8(std::mem::take(raw))
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "CSV field is not valid UTF-8"))?;
    fields.push(Field { text: s, quoted });
    Ok(())
}

pub struct CsvWriter<W: Write> {
    inner: W,
}

impl<W: Write> CsvWriter<W> {
    pub fn new(inner: W) -> Self {
        CsvWriter { inner }
    }

    /// Write one record. `force_quote` quotes a field even when unnecessary
    /// (used to write empty-string as "" vs NULL as empty).
    pub fn write_record<'a>(
        &mut self,
        fields: impl Iterator<Item = (&'a str, bool)>,
    ) -> std::io::Result<()> {
        let mut first = true;
        for (text, force_quote) in fields {
            if !first {
                self.inner.write_all(b",")?;
            }
            first = false;
            let needs_quote = force_quote
                || text.contains(',')
                || text.contains('"')
                || text.contains('\n')
                || text.contains('\r');
            if needs_quote {
                self.inner.write_all(b"\"")?;
                self.inner.write_all(text.replace('"', "\"\"").as_bytes())?;
                self.inner.write_all(b"\"")?;
            } else {
                self.inner.write_all(text.as_bytes())?;
            }
        }
        self.inner.write_all(b"\n")
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn read_all(s: &str) -> Vec<Vec<Field>> {
        let mut r = CsvReader::new(BufReader::new(s.as_bytes()));
        let mut out = Vec::new();
        while let Some(rec) = r.next_record().unwrap() {
            out.push(rec);
        }
        out
    }

    #[test]
    fn basic() {
        let recs = read_all("a,b,c\n1,2,3\n");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[1][2].text, "3");
        assert!(!recs[1][2].quoted);
    }

    #[test]
    fn quoting() {
        let recs = read_all("\"a,b\",\"say \"\"hi\"\"\",\"multi\nline\"\n\"\",x\n");
        assert_eq!(recs[0][0].text, "a,b");
        assert_eq!(recs[0][1].text, "say \"hi\"");
        assert_eq!(recs[0][2].text, "multi\nline");
        assert_eq!(recs[1][0].text, "");
        assert!(recs[1][0].quoted); // "" — empty string, not null
        assert!(!recs[1][1].quoted);
    }

    #[test]
    fn crlf_and_no_trailing_newline() {
        let recs = read_all("a,b\r\n1,2");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[1][1].text, "2");
    }

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        {
            let mut w = CsvWriter::new(&mut buf);
            w.write_record([("plain", false), ("with,comma", false), ("", true), ("", false)].into_iter())
                .unwrap();
            w.write_record([("line\nbreak", false), ("q\"uote", false), ("x", false), ("y", false)].into_iter())
                .unwrap();
        }
        let recs = read_all(std::str::from_utf8(&buf).unwrap());
        assert_eq!(recs[0][1].text, "with,comma");
        assert!(recs[0][2].quoted);
        assert!(!recs[0][3].quoted);
        assert_eq!(recs[1][0].text, "line\nbreak");
        assert_eq!(recs[1][1].text, "q\"uote");
    }

    #[test]
    fn unterminated_quote_errors() {
        let mut r = CsvReader::new(BufReader::new("\"abc".as_bytes()));
        assert!(r.next_record().is_err());
    }
}
