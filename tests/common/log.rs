//! The server's own log, captured in memory so that a test can read a line the
//! server writes rather than recompute the number in it.
//!
//! The subscriber is process-global, so the first call installs it for the
//! whole test binary. A `Game_ID` is minted per server and every test starts
//! its own, so two games in one binary can carry the same identifier; what
//! separates them is when they were logged, which is why a test reads only what
//! was appended since an offset it took first.

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;

/// Installs the capture if no call has yet. Call it before anything that logs
/// a line a test is going to read.
pub fn install() {
    let _ = buffer();
}

/// How many bytes have been logged so far, as an offset into the buffer.
pub fn logged_so_far() -> usize {
    buffer()
        .lock()
        .expect("the log buffer is not poisoned")
        .len()
}

/// The lines logged since `offset`.
pub fn logged_since(offset: usize) -> Vec<String> {
    let buffer = buffer();
    let bytes = buffer.lock().expect("the log buffer is not poisoned")[offset..].to_vec();

    String::from_utf8(bytes)
        .expect("tracing writes UTF-8")
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn buffer() -> Arc<Mutex<Vec<u8>>> {
    static BUFFER: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

    Arc::clone(BUFFER.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(Capture(Arc::clone(&buffer)))
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other subscriber is installed in this binary");

        buffer
    }))
}

/// A `MakeWriter` over the shared buffer.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log buffer is not poisoned")
            .extend_from_slice(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
