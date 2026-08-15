//! Streaming COPY over dedicated connections.
//!
//! The driver's COPY reader and writer borrow the client for as long as the
//! copy runs, which the `RowSource`/`RowSink` trait objects the engine holds
//! cannot express. Each copy therefore owns a connection on its own thread and
//! exchanges lines over a bounded channel — which also overlaps network I/O
//! with masking work.

use super::{describe, PgUrl};
use crate::{rerr, Result};
use std::io::{BufRead, Write};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::JoinHandle;

/// Lines buffered between a copy thread and the engine.
const QUEUE: usize = 1024;

// ---------- COPY OUT ----------

pub struct CopyOut {
    lines: Receiver<Result<Vec<u8>>>,
    worker: Option<JoinHandle<()>>,
}

impl CopyOut {
    pub fn start(url: &PgUrl, sql: String) -> Result<CopyOut> {
        let url = url.clone();
        let (tx, lines) = sync_channel(QUEUE);
        let worker = std::thread::Builder::new()
            .name("altonic-copy-out".to_string())
            .spawn(move || {
                if let Err(e) = pump_out(&url, &sql, &tx) {
                    let _ = tx.send(Err(e));
                }
            })
            .map_err(|e| rerr(format!("cannot spawn COPY reader thread: {e}")))?;
        Ok(CopyOut { lines, worker: Some(worker) })
    }

    /// The next COPY line without its newline, or None at end of data.
    pub fn next_line(&mut self) -> Result<Option<Vec<u8>>> {
        match self.lines.recv() {
            Ok(line) => line.map(Some),
            // The sender is gone: the copy finished, or the thread died.
            Err(_) => {
                self.join()?;
                Ok(None)
            }
        }
    }

    fn join(&mut self) -> Result<()> {
        match self.worker.take() {
            Some(h) => h.join().map_err(|_| rerr("postgres: COPY reader thread panicked")),
            None => Ok(()),
        }
    }
}

fn pump_out(url: &PgUrl, sql: &str, tx: &SyncSender<Result<Vec<u8>>>) -> Result<()> {
    let mut client = url.connect()?;
    let mut reader = client
        .copy_out(sql)
        .map_err(|e| rerr(format!("postgres: {}\n  while running: {sql}", describe(&e))))?;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| rerr(format!("postgres: COPY OUT failed: {e}")))?;
        if n == 0 {
            return Ok(());
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line == b"\\." {
            continue; // legacy end marker, never data (a real value escapes to \\.)
        }
        if tx.send(Ok(std::mem::take(&mut line))).is_err() {
            return Ok(()); // consumer went away
        }
    }
}

// ---------- COPY IN ----------

enum Msg {
    Line(Vec<u8>),
    /// Explicit completion. Without it the copy is aborted rather than
    /// committing a half-written table.
    Done,
}

pub struct CopyIn {
    tx: Option<SyncSender<Msg>>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl CopyIn {
    pub fn start(url: &PgUrl, sql: String) -> Result<CopyIn> {
        let url = url.clone();
        let (tx, rx) = sync_channel(QUEUE);
        let worker = std::thread::Builder::new()
            .name("altonic-copy-in".to_string())
            .spawn(move || pump_in(&url, &sql, rx))
            .map_err(|e| rerr(format!("cannot spawn COPY writer thread: {e}")))?;
        Ok(CopyIn { tx: Some(tx), worker: Some(worker) })
    }

    pub fn write_line(&mut self, line: &[u8]) -> Result<()> {
        let Some(tx) = self.tx.as_ref() else {
            return Err(rerr("postgres: COPY IN already finished"));
        };
        if tx.send(Msg::Line(line.to_vec())).is_err() {
            // The writer thread failed; it holds the real error.
            self.tx = None;
            return self.join();
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Done);
        }
        self.join()
    }

    fn join(&mut self) -> Result<()> {
        match self.worker.take() {
            Some(h) => h
                .join()
                .map_err(|_| rerr("postgres: COPY writer thread panicked"))?,
            None => Ok(()),
        }
    }
}

fn pump_in(url: &PgUrl, sql: &str, rx: Receiver<Msg>) -> Result<()> {
    let mut client = url.connect()?;
    let mut writer = client
        .copy_in(sql)
        .map_err(|e| rerr(format!("postgres: {}\n  while running: {sql}", describe(&e))))?;
    let mut done = false;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Line(line) => {
                writer
                    .write_all(&line)
                    .and_then(|()| writer.write_all(b"\n"))
                    .map_err(|e| rerr(format!("postgres: COPY IN failed: {e}")))?;
            }
            Msg::Done => {
                done = true;
                break;
            }
        }
    }
    if !done {
        // Dropping the writer without finishing aborts the copy server-side.
        return Err(rerr("postgres: COPY IN abandoned before completion"));
    }
    writer
        .finish()
        .map_err(|e| rerr(format!("postgres: {} during COPY IN", describe(&e))))?;
    Ok(())
}
