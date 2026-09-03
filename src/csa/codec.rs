//! Line framing: bounded reads and terminated writes. Nothing here interprets
//! a line; that is the command layer's job.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Longest `LOGIN` line the protocol can legitimately carry, in bytes. Both
/// fields are counted in characters, and a character is a byte for both:
/// engine names are `[A-Za-z0-9_@\-\.]` and `open`-mode tokens are printable
/// ASCII.
const MAX_LOGIN_LINE_LEN: usize = "LOGIN ".len() + 1024 + " ".len() + 64;

/// Maximum length of one CSA line, terminator excluded.
///
/// Not the round 1024 a reader would expect: a maximal `LOGIN` line runs past
/// 1 KB, so that cap would reject a login the protocol allows.
pub const MAX_LINE_LEN: usize = 2048;

const _: () = assert!(MAX_LINE_LEN >= MAX_LOGIN_LINE_LEN);

/// Bytes read for one line before the reader gives up: a maximal line plus the
/// longest terminator it may carry (CR LF). Reading one byte past a full line
/// distinguishes "exactly at the cap" from "over it" without ever holding an
/// over-long line.
const READ_LIMIT: usize = MAX_LINE_LEN + 2;

/// A framing failure. Every variant is fatal to the connection that produced
/// it: the stream position after an over-long or truncated line is no longer a
/// line boundary, so there is nothing to resynchronize to.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("line exceeds the {} byte cap", MAX_LINE_LEN)]
    LineTooLong,

    #[error("line is not valid UTF-8")]
    InvalidUtf8,

    #[error("stream ended before a line terminator")]
    UnexpectedEof,

    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reads CSA lines from a buffered stream, bounded before allocation.
///
/// A line ends at LF. An immediately preceding CR is tolerated and stripped,
/// and the returned line carries neither byte. No specification text governs
/// the terminator; this follows shogi-server, which reads LF-delimited lines
/// and strips the trailing CR/LF cluster (`gets_safe` and the read loop in its
/// main server script).
pub struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
}

/// Hand-written because `buf` transiently holds a whole `LOGIN` line — engine
/// name and token — and a derived `Debug` would print it. No credential
/// material in a rendering.
impl<R> std::fmt::Debug for LineReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineReader")
            .field("buffered", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    /// Wraps a buffered stream.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    /// Reads the next line, without its terminator.
    ///
    /// Returns `Ok(None)` when the stream ends cleanly on a line boundary.
    ///
    /// At most [`READ_LIMIT`] bytes are read per call, so a client that never
    /// sends a terminator is refused rather than growing a buffer until it
    /// decides to stop.
    pub async fn read_line(&mut self) -> Result<Option<&str>, Error> {
        self.buf.clear();
        let read = (&mut self.inner)
            .take(READ_LIMIT as u64)
            .read_until(b'\n', &mut self.buf)
            .await?;

        if read == 0 {
            return Ok(None);
        }

        let Some(line) = self.buf.strip_suffix(b"\n") else {
            return Err(if read == READ_LIMIT {
                Error::LineTooLong
            } else {
                Error::UnexpectedEof
            });
        };
        let line = line.strip_suffix(b"\r").unwrap_or(line);

        // A terminated line can still overrun: the cap counts content, and the
        // read limit allows room for the terminator on top of it.
        if line.len() > MAX_LINE_LEN {
            return Err(Error::LineTooLong);
        }

        std::str::from_utf8(line)
            .map(Some)
            .map_err(|_| Error::InvalidUtf8)
    }

    /// Returns the wrapped stream.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

/// Writes CSA lines, each terminated with LF.
///
/// Flushing is not offered: when a line reaches the wire is a relay decision,
/// so callers reach the stream through [`LineWriter::get_mut`] and flush it
/// themselves.
pub struct LineWriter<W> {
    inner: W,
    buf: Vec<u8>,
}

/// Hand-written for the same reason as [`LineReader`]'s.
impl<W> std::fmt::Debug for LineWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineWriter")
            .field("buffered", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl<W: AsyncWrite + Unpin> LineWriter<W> {
    /// Wraps a stream.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    /// Writes `line` followed by LF, and nothing else.
    ///
    /// `line` must carry no terminator of its own; the codec adds the only
    /// one, in the same write, so a line cannot reach the wire unterminated.
    pub async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        debug_assert!(
            !line.contains('\n'),
            "write_line adds the only terminator; `line` must carry none"
        );

        self.buf.clear();
        self.buf.reserve(line.len() + 1);
        self.buf.extend_from_slice(line.as_bytes());
        self.buf.push(b'\n');
        self.inner.write_all(&self.buf).await?;
        Ok(())
    }

    /// Borrows the wrapped stream, for flushing and for shutdown.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Returns the wrapped stream.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::AsyncRead;

    use super::*;

    async fn read_one(input: &[u8]) -> Result<Option<String>, Error> {
        LineReader::new(input)
            .read_line()
            .await
            .map(|line| line.map(str::to_owned))
    }

    /// Serves `b'a'` up to `remaining` bytes and counts what a reader consumed,
    /// so an unbounded reader fails a test instead of hanging it.
    struct Endless {
        chunk: [u8; 64],
        remaining: usize,
        consumed: usize,
    }

    impl Endless {
        fn new(remaining: usize) -> Self {
            Self {
                chunk: [b'a'; 64],
                remaining,
                consumed: 0,
            }
        }

        fn available(&self) -> &[u8] {
            &self.chunk[..self.remaining.min(self.chunk.len())]
        }
    }

    impl AsyncRead for Endless {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let n = this.available().len().min(buf.remaining());
            buf.put_slice(&this.chunk[..n]);
            this.remaining -= n;
            this.consumed += n;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncBufRead for Endless {
        fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
            Poll::Ready(Ok(self.get_mut().available()))
        }

        fn consume(self: Pin<&mut Self>, amt: usize) {
            let this = self.get_mut();
            this.remaining -= amt;
            this.consumed += amt;
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn reads_an_lf_terminated_line() {
        assert_eq!(
            read_one(b"LOGOUT\n").await.unwrap().as_deref(),
            Some("LOGOUT")
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn reads_a_crlf_terminated_line_without_either_terminator() {
        assert_eq!(
            read_one(b"LOGOUT\r\n").await.unwrap().as_deref(),
            Some("LOGOUT")
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn strips_only_the_cr_that_precedes_the_lf() {
        // A CR anywhere else is content, not framing.
        assert_eq!(
            read_one(b"AB\rCD\n").await.unwrap().as_deref(),
            Some("AB\rCD")
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn reads_an_empty_line() {
        assert_eq!(read_one(b"\n").await.unwrap().as_deref(), Some(""));
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn reads_successive_lines_then_reports_end_of_stream() {
        let input = b"+7776FU,T10\n-3334FU,T12\n";
        let mut reader = LineReader::new(&input[..]);

        assert_eq!(reader.read_line().await.unwrap(), Some("+7776FU,T10"));
        assert_eq!(reader.read_line().await.unwrap(), Some("-3334FU,T12"));
        assert_eq!(reader.read_line().await.unwrap(), None);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn reports_end_of_stream_on_an_empty_stream() {
        assert_eq!(read_one(b"").await.unwrap(), None);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn accepts_a_line_of_exactly_the_cap() {
        let mut input = vec![b'a'; MAX_LINE_LEN];
        input.push(b'\n');

        let line = read_one(&input).await.unwrap().unwrap();
        assert_eq!(line.len(), MAX_LINE_LEN);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn accepts_a_line_of_exactly_the_cap_with_crlf() {
        let mut input = vec![b'a'; MAX_LINE_LEN];
        input.extend_from_slice(b"\r\n");

        let line = read_one(&input).await.unwrap().unwrap();
        assert_eq!(line.len(), MAX_LINE_LEN);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn rejects_a_line_one_byte_over_the_cap() {
        let mut input = vec![b'a'; MAX_LINE_LEN + 1];
        input.push(b'\n');

        assert!(matches!(read_one(&input).await, Err(Error::LineTooLong)));
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn accepts_a_maximal_login_line() {
        let name = "a".repeat(1024);
        let token = "b".repeat(64);
        let input = format!("LOGIN {name} {token}\n");
        assert!(input.len() - 1 > 1024);

        let line = read_one(input.as_bytes()).await.unwrap().unwrap();
        assert_eq!(line.len(), MAX_LOGIN_LINE_LEN);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn refuses_a_terminator_free_stream_after_a_bounded_read() {
        let mut reader = LineReader::new(Endless::new(1 << 20));

        assert!(matches!(reader.read_line().await, Err(Error::LineTooLong)));

        let consumed = reader.into_inner().consumed;
        assert_eq!(
            consumed, READ_LIMIT,
            "the flood must be refused after cap plus margin, not after buffering it"
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn rejects_invalid_utf8() {
        assert!(matches!(
            read_one(b"LOGIN \xff\xfe x\n").await,
            Err(Error::InvalidUtf8)
        ));
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn accepts_multi_byte_utf8() {
        assert_eq!(
            read_one("'コメント\n".as_bytes()).await.unwrap().as_deref(),
            Some("'コメント")
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn rejects_a_line_truncated_by_end_of_stream() {
        assert!(matches!(
            read_one(b"LOGOU").await,
            Err(Error::UnexpectedEof)
        ));
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn debug_omits_the_line_just_read() {
        let mut reader = LineReader::new(&b"LOGIN engine-1 s3cret-token\n"[..]);
        let line = reader.read_line().await.unwrap().unwrap().to_owned();

        let debug = format!("{reader:?}");
        assert!(
            !debug.contains(&line),
            "Debug leaked the buffered line: {debug}"
        );
        assert!(!debug.contains("s3cret-token"), "Debug leaked a token");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn writes_a_line_followed_by_lf() {
        let mut writer = LineWriter::new(Vec::new());

        writer.write_line("LOGIN:test OK").await.unwrap();

        assert_eq!(writer.into_inner(), b"LOGIN:test OK\n");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn writes_successive_lines_with_nothing_between_them() {
        let mut writer = LineWriter::new(Vec::new());

        writer.write_line("BEGIN Game_Summary").await.unwrap();
        writer.write_line("").await.unwrap();
        writer.write_line("END Game_Summary").await.unwrap();

        assert_eq!(
            writer.into_inner(),
            b"BEGIN Game_Summary\n\nEND Game_Summary\n"
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn round_trips_a_written_line_through_the_reader() {
        let mut writer = LineWriter::new(Vec::new());
        writer.write_line("+7776FU,T10").await.unwrap();
        let written = writer.into_inner();

        assert_eq!(
            read_one(&written).await.unwrap().as_deref(),
            Some("+7776FU,T10")
        );
    }
}
