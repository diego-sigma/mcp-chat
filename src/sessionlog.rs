use std::fs::{OpenOptions, create_dir_all};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{Value, json};

/// JSONL session log. Each call to a log method appends one line containing
/// `{ts, event, ...payload}`. Best-effort: write failures are dropped so a
/// broken disk doesn't kill the REPL.
///
/// The backing writer is pluggable — `open()` writes to a file on disk,
/// `open_buffer()` writes to an in-memory buffer (used for the one-shot
/// `--from-log` + `--prompt` mode which captures the new turn to emit as
/// JSON instead of persisting it).
pub struct SessionLog {
    inner: Mutex<Box<dyn Write + Send>>,
    pub path: Option<PathBuf>,
}

/// `Write` adapter that funnels bytes into a shared `Vec<u8>`. The holder of
/// the cloned `Arc` can read the accumulated bytes once the log is dropped
/// (or while it's idle).
#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut v) = self.0.lock() {
            v.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `Write` fan-out that mirrors every write to both an underlying writer
/// (typically a file) and a shared in-memory buffer. Used by the one-shot
/// mode with `--session-id`: the session log persists AND the new-turn
/// bytes are captured for the stdout JSON.
struct TeeWrite {
    file: Box<dyn Write + Send>,
    buf: SharedBuffer,
}

impl Write for TeeWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write_all(buf)?;
        let _ = self.buf.write(buf); // buffer write is infallible in practice
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl SessionLog {
    pub fn open(dir: &Path, model: &str) -> Result<Self> {
        create_dir_all(dir).with_context(|| format!("create log dir {}", dir.display()))?;
        let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
        let model_slug: String = model
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
            .collect();
        let path = dir.join(format!("session-{ts}-{model_slug}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open log file {}", path.display()))?;
        Ok(Self {
            inner: Mutex::new(Box::new(file)),
            path: Some(path),
        })
    }

    /// Returns a SessionLog whose writes accumulate in a shared in-memory
    /// buffer. Hold the returned `Arc<Mutex<Vec<u8>>>` to read the captured
    /// bytes back (typically after dropping the SessionLog).
    pub fn open_buffer() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedBuffer(buf.clone());
        let log = Self {
            inner: Mutex::new(Box::new(sink)),
            path: None,
        };
        (log, buf)
    }

    /// Deterministic filename form: `<dir>/session-<id>.jsonl`. Opens in
    /// append mode, so repeated calls with the same id grow one log file.
    /// Callers that want to append to a session across invocations pass
    /// the same id every time.
    pub fn open_with_id(dir: &Path, id: &str) -> Result<Self> {
        create_dir_all(dir).with_context(|| format!("create log dir {}", dir.display()))?;
        let path = dir.join(format!("session-{id}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open log file {}", path.display()))?;
        Ok(Self {
            inner: Mutex::new(Box::new(file)),
            path: Some(path),
        })
    }

    /// Same as `open_with_id` but also mirrors every write into an in-memory
    /// buffer so the one-shot mode can emit the new turn to stdout while
    /// persisting it to disk.
    pub fn open_with_id_tee(dir: &Path, id: &str) -> Result<(Self, Arc<Mutex<Vec<u8>>>)> {
        create_dir_all(dir).with_context(|| format!("create log dir {}", dir.display()))?;
        let path = dir.join(format!("session-{id}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open log file {}", path.display()))?;
        let buf = Arc::new(Mutex::new(Vec::new()));
        let tee = TeeWrite {
            file: Box::new(file),
            buf: SharedBuffer(buf.clone()),
        };
        Ok((
            Self {
                inner: Mutex::new(Box::new(tee)),
                path: Some(path),
            },
            buf,
        ))
    }

    fn write(&self, mut event: Value) {
        if let Value::Object(ref mut m) = event {
            m.insert("ts".into(), Value::String(Utc::now().to_rfc3339()));
        }
        let Ok(mut line) = serde_json::to_string(&event) else {
            return;
        };
        line.push('\n');
        if let Ok(mut f) = self.inner.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    pub fn session_start(&self, model: &str, base_url: &str, system_prompt: Option<&str>) {
        self.write(json!({
            "event": "session_start",
            "model": model,
            "base_url": base_url,
            "system_prompt": system_prompt,
        }));
    }

    pub fn user(&self, content: &str) {
        self.write(json!({ "event": "user", "content": content }));
    }

    pub fn assistant(&self, content: Option<&str>, tool_calls: &Value) {
        self.write(json!({
            "event": "assistant",
            "content": content,
            "tool_calls": tool_calls,
        }));
    }

    pub fn tool_call(&self, id: &str, name: &str, arguments: &str) {
        self.write(json!({
            "event": "tool_call",
            "id": id,
            "name": name,
            "arguments": arguments,
        }));
    }

    pub fn tool_result(&self, id: &str, content: &str, is_error: bool) {
        self.write(json!({
            "event": "tool_result",
            "tool_call_id": id,
            "content": content,
            "is_error": is_error,
        }));
    }

    pub fn error(&self, message: &str) {
        self.write(json!({ "event": "error", "message": message }));
    }

    pub fn session_end(&self) {
        self.write(json!({ "event": "session_end" }));
    }
}
