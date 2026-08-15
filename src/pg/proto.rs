//! Postgres frontend/backend wire protocol v3 over plain TCP, from the
//! documented message formats. Implements exactly what the connectors need:
//! startup + password auth (cleartext, md5, SCRAM-SHA-256), simple queries
//! with text results, and COPY in both directions.
//!
//! TLS is not implemented; URLs requesting `sslmode=require` are rejected
//! up front rather than silently downgraded.

use super::md5::pg_md5_password;
use super::scram::ScramClient;
use super::url::PgUrl;
use crate::{rerr, Result};
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;

pub struct PgConn {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    /// Leftover COPY-out bytes not yet split into lines.
    copy_buf: Vec<u8>,
}

pub struct Backend {
    pub tag: u8,
    pub payload: Vec<u8>,
}

fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> Result<()> {
    r.read_exact(buf).map_err(|e| rerr(format!("postgres connection closed: {e}")))
}

fn cstr(payload: &[u8], at: usize) -> Result<(String, usize)> {
    let end = payload[at..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| rerr("postgres: unterminated string in message"))?;
    let s = String::from_utf8(payload[at..at + end].to_vec())
        .map_err(|_| rerr("postgres: non-UTF-8 string in message"))?;
    Ok((s, at + end + 1))
}

/// Extract the human-readable message from an ErrorResponse payload.
fn error_text(payload: &[u8]) -> String {
    let mut severity = String::new();
    let mut message = String::from("unknown error");
    let mut detail = String::new();
    let mut i = 0;
    while i < payload.len() && payload[i] != 0 {
        let field = payload[i];
        if let Ok((text, next)) = cstr(payload, i + 1) {
            match field {
                b'S' => severity = text,
                b'M' => message = text,
                b'D' => detail = format!(" ({text})"),
                _ => {}
            }
            i = next;
        } else {
            break;
        }
    }
    if severity.is_empty() {
        format!("{message}{detail}")
    } else {
        format!("{severity}: {message}{detail}")
    }
}

fn os_entropy() -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 32];
        if f.read_exact(&mut buf).is_ok() {
            out.extend_from_slice(&buf);
        }
    }
    // Mix in ambient values in case /dev/urandom is unavailable.
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        out.extend_from_slice(&d.as_nanos().to_be_bytes());
    }
    out.extend_from_slice(&std::process::id().to_be_bytes());
    out
}

impl PgConn {
    pub fn connect(url: &PgUrl) -> Result<PgConn> {
        let addr = format!("{}:{}", url.host, url.port);
        let stream = TcpStream::connect(&addr)
            .map_err(|e| rerr(format!("cannot connect to postgres at {addr}: {e}")))?;
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(
            stream.try_clone().map_err(|e| rerr(format!("socket clone failed: {e}")))?,
        );
        let writer = BufWriter::new(stream);
        let mut conn = PgConn { reader, writer, copy_buf: Vec::new() };
        conn.startup(url)?;
        // Pin the session to UTC so timestamptz text is stable (+00) and
        // shift arithmetic in datetime generators stays correct.
        conn.simple_query("SET TIME ZONE 'UTC'")?;
        Ok(conn)
    }

    fn startup(&mut self, url: &PgUrl) -> Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(&196608i32.to_be_bytes()); // protocol 3.0
        for (k, v) in [
            ("user", url.user.as_str()),
            ("database", url.database.as_str()),
            ("client_encoding", "UTF8"),
            ("application_name", "altonic"),
        ] {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let len = (body.len() + 4) as i32;
        self.writer.write_all(&len.to_be_bytes()).map_err(io_err)?;
        self.writer.write_all(&body).map_err(io_err)?;
        self.writer.flush().map_err(io_err)?;

        // Authentication loop.
        loop {
            let msg = self.read_message()?;
            match msg.tag {
                b'R' => {
                    let code = i32::from_be_bytes(
                        msg.payload
                            .get(0..4)
                            .ok_or_else(|| rerr("postgres: short auth message"))?
                            .try_into()
                            .unwrap(),
                    );
                    match code {
                        0 => {} // AuthenticationOk
                        3 => self.send_password(url.password()?.as_str())?,
                        5 => {
                            let salt: [u8; 4] = msg
                                .payload
                                .get(4..8)
                                .ok_or_else(|| rerr("postgres: md5 auth without salt"))?
                                .try_into()
                                .unwrap();
                            self.send_password(&pg_md5_password(
                                &url.user,
                                &url.password()?,
                                &salt,
                            ))?;
                        }
                        10 => self.scram_auth(url, &msg.payload[4..])?,
                        7..=9 => {
                            return Err(rerr(
                                "postgres: GSSAPI/SSPI authentication is not supported; use scram-sha-256, md5, or password auth",
                            ))
                        }
                        other => {
                            return Err(rerr(format!(
                                "postgres: unsupported authentication request {other}"
                            )))
                        }
                    }
                }
                b'S' | b'K' | b'N' => {} // ParameterStatus / BackendKeyData / Notice
                b'Z' => return Ok(()),   // ReadyForQuery
                b'E' => return Err(rerr(format!("postgres: {}", error_text(&msg.payload)))),
                other => {
                    return Err(rerr(format!(
                        "postgres: unexpected message {:?} during startup",
                        other as char
                    )))
                }
            }
        }
    }

    fn scram_auth(&mut self, url: &PgUrl, mechanisms: &[u8]) -> Result<()> {
        // Mechanism list: cstrings terminated by an empty string.
        let mut offered = Vec::new();
        let mut i = 0;
        while i < mechanisms.len() && mechanisms[i] != 0 {
            let (m, next) = cstr(mechanisms, i)?;
            offered.push(m);
            i = next;
        }
        if !offered.iter().any(|m| m == "SCRAM-SHA-256") {
            return Err(rerr(format!(
                "postgres: server offers {offered:?}; only SCRAM-SHA-256 is supported (SCRAM-SHA-256-PLUS needs TLS)"
            )));
        }
        let mut scram = ScramClient::new(&url.password()?, &os_entropy());

        // SASLInitialResponse
        let first = scram.client_first();
        let mut body = Vec::new();
        body.extend_from_slice(b"SCRAM-SHA-256\0");
        body.extend_from_slice(&(first.len() as i32).to_be_bytes());
        body.extend_from_slice(first.as_bytes());
        self.send_frontend(b'p', &body)?;

        // AuthenticationSASLContinue
        let cont = self.expect_auth(11)?;
        let server_first = String::from_utf8(cont)
            .map_err(|_| rerr("postgres: non-UTF-8 SCRAM challenge"))?;
        let final_msg = scram.client_final(&server_first)?;
        self.send_frontend(b'p', final_msg.as_bytes())?;

        // AuthenticationSASLFinal
        let fin = self.expect_auth(12)?;
        let server_final =
            String::from_utf8(fin).map_err(|_| rerr("postgres: non-UTF-8 SCRAM final"))?;
        scram.verify_server_final(&server_final)?;
        Ok(())
    }

    fn expect_auth(&mut self, want: i32) -> Result<Vec<u8>> {
        loop {
            let msg = self.read_message()?;
            match msg.tag {
                b'R' => {
                    let code =
                        i32::from_be_bytes(msg.payload[0..4].try_into().unwrap());
                    if code != want {
                        return Err(rerr(format!(
                            "postgres: expected auth continuation {want}, got {code}"
                        )));
                    }
                    return Ok(msg.payload[4..].to_vec());
                }
                b'E' => return Err(rerr(format!("postgres: {}", error_text(&msg.payload)))),
                b'N' => {}
                other => {
                    return Err(rerr(format!(
                        "postgres: unexpected message {:?} during SASL",
                        other as char
                    )))
                }
            }
        }
    }

    fn send_password(&mut self, password: &str) -> Result<()> {
        let mut body = password.as_bytes().to_vec();
        body.push(0);
        self.send_frontend(b'p', &body)
    }

    fn send_frontend(&mut self, tag: u8, body: &[u8]) -> Result<()> {
        self.writer.write_all(&[tag]).map_err(io_err)?;
        self.writer.write_all(&((body.len() + 4) as i32).to_be_bytes()).map_err(io_err)?;
        self.writer.write_all(body).map_err(io_err)?;
        self.writer.flush().map_err(io_err)
    }

    fn read_message(&mut self) -> Result<Backend> {
        let mut head = [0u8; 5];
        read_exact(&mut self.reader, &mut head)?;
        let tag = head[0];
        let len = i32::from_be_bytes(head[1..5].try_into().unwrap());
        if !(4..=1 << 30).contains(&len) {
            return Err(rerr(format!("postgres: invalid message length {len}")));
        }
        let mut payload = vec![0u8; (len - 4) as usize];
        read_exact(&mut self.reader, &mut payload)?;
        Ok(Backend { tag, payload })
    }

    /// Run a simple query; return the text rows (None = NULL per cell).
    /// Multiple statements are allowed; rows from all result sets concatenate.
    pub fn simple_query(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send_frontend(b'Q', &body)?;
        let mut rows = Vec::new();
        let mut error: Option<String> = None;
        loop {
            let msg = self.read_message()?;
            match msg.tag {
                b'T' | b'C' | b'I' | b'S' | b'N' | b'A' => {}
                b'D' => rows.push(parse_data_row(&msg.payload)?),
                b'E' => error = Some(error_text(&msg.payload)),
                b'Z' => {
                    return match error {
                        Some(e) => Err(rerr(format!("postgres: {e}\n  while running: {sql}"))),
                        None => Ok(rows),
                    }
                }
                b'G' | b'H' => {
                    return Err(rerr(format!(
                        "postgres: unexpected COPY response to simple query: {sql}"
                    )))
                }
                other => {
                    return Err(rerr(format!(
                        "postgres: unexpected message {:?} in query response",
                        other as char
                    )))
                }
            }
        }
    }

    /// Convenience: run and expect a single text column.
    pub fn query_strings(&mut self, sql: &str) -> Result<Vec<String>> {
        Ok(self
            .simple_query(sql)?
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .collect())
    }

    // ---- COPY OUT ----

    /// Start `COPY ... TO STDOUT`; then call `copy_out_line` until None.
    pub fn copy_out_start(&mut self, sql: &str) -> Result<()> {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send_frontend(b'Q', &body)?;
        loop {
            let msg = self.read_message()?;
            match msg.tag {
                b'H' => {
                    self.copy_buf.clear();
                    return Ok(());
                }
                b'S' | b'N' => {}
                b'E' => {
                    let e = error_text(&msg.payload);
                    self.drain_to_ready()?;
                    return Err(rerr(format!("postgres: {e}\n  while running: {sql}")));
                }
                other => {
                    return Err(rerr(format!(
                        "postgres: unexpected message {:?} starting COPY OUT",
                        other as char
                    )))
                }
            }
        }
    }

    /// Next line of COPY text output (without trailing newline), or None when
    /// the COPY has finished (connection drained to ReadyForQuery).
    pub fn copy_out_line(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(pos) = self.copy_buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.copy_buf.drain(..=pos).collect();
                line.pop(); // newline
                if line == b"\\." {
                    continue; // legacy end marker, not data
                }
                return Ok(Some(line));
            }
            let msg = self.read_message()?;
            match msg.tag {
                b'd' => self.copy_buf.extend_from_slice(&msg.payload),
                b'c' => {
                    // CopyDone; drain CommandComplete + ReadyForQuery.
                    self.drain_to_ready()?;
                    if !self.copy_buf.is_empty() {
                        return Err(rerr("postgres: COPY OUT ended mid-line"));
                    }
                    return Ok(None);
                }
                b'N' | b'S' => {}
                b'E' => {
                    let e = error_text(&msg.payload);
                    self.drain_to_ready()?;
                    return Err(rerr(format!("postgres: {e} during COPY OUT")));
                }
                other => {
                    return Err(rerr(format!(
                        "postgres: unexpected message {:?} during COPY OUT",
                        other as char
                    )))
                }
            }
        }
    }

    // ---- COPY IN ----

    /// Start `COPY ... FROM STDIN`.
    pub fn copy_in_start(&mut self, sql: &str) -> Result<()> {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send_frontend(b'Q', &body)?;
        loop {
            let msg = self.read_message()?;
            match msg.tag {
                b'G' => return Ok(()),
                b'S' | b'N' => {}
                b'E' => {
                    let e = error_text(&msg.payload);
                    self.drain_to_ready()?;
                    return Err(rerr(format!("postgres: {e}\n  while running: {sql}")));
                }
                other => {
                    return Err(rerr(format!(
                        "postgres: unexpected message {:?} starting COPY IN",
                        other as char
                    )))
                }
            }
        }
    }

    pub fn copy_in_data(&mut self, line: &[u8]) -> Result<()> {
        // One CopyData frame per line (payload includes the newline).
        self.writer.write_all(b"d").map_err(io_err)?;
        self.writer.write_all(&((line.len() + 5) as i32).to_be_bytes()).map_err(io_err)?;
        self.writer.write_all(line).map_err(io_err)?;
        self.writer.write_all(b"\n").map_err(io_err)?;
        Ok(())
    }

    pub fn copy_in_finish(&mut self) -> Result<()> {
        self.send_frontend(b'c', &[])?; // CopyDone
        let mut error = None;
        loop {
            let msg = self.read_message()?;
            match msg.tag {
                b'C' | b'S' | b'N' => {}
                b'E' => error = Some(error_text(&msg.payload)),
                b'Z' => {
                    return match error {
                        Some(e) => Err(rerr(format!("postgres: {e} during COPY IN"))),
                        None => Ok(()),
                    }
                }
                other => {
                    return Err(rerr(format!(
                        "postgres: unexpected message {:?} finishing COPY IN",
                        other as char
                    )))
                }
            }
        }
    }

    fn drain_to_ready(&mut self) -> Result<()> {
        loop {
            // Swallow everything until ReadyForQuery, including further errors.
            if self.read_message()?.tag == b'Z' {
                return Ok(());
            }
        }
    }
}

fn parse_data_row(payload: &[u8]) -> Result<Vec<Option<String>>> {
    if payload.len() < 2 {
        return Err(rerr("postgres: short DataRow"));
    }
    let n = i16::from_be_bytes(payload[0..2].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(n);
    let mut i = 2;
    for _ in 0..n {
        let len = i32::from_be_bytes(
            payload
                .get(i..i + 4)
                .ok_or_else(|| rerr("postgres: truncated DataRow"))?
                .try_into()
                .unwrap(),
        );
        i += 4;
        if len < 0 {
            out.push(None);
        } else {
            let len = len as usize;
            let bytes = payload
                .get(i..i + len)
                .ok_or_else(|| rerr("postgres: truncated DataRow field"))?;
            out.push(Some(
                String::from_utf8(bytes.to_vec())
                    .map_err(|_| rerr("postgres: non-UTF-8 field"))?,
            ));
            i += len;
        }
    }
    Ok(out)
}

fn io_err(e: std::io::Error) -> crate::Error {
    rerr(format!("postgres connection error: {e}"))
}
