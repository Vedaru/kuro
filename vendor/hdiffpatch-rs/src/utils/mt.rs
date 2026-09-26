use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

#[derive(Debug, Clone, Copy)]
pub struct PipelineConfig {
    pub threads: usize,
    pub buf_size: usize,
    pub depth: usize,
}

impl PipelineConfig {
    pub fn from_budget(threads: usize, budget: usize, buf_size: usize) -> Self {
        let threads = threads.max(1);
        if threads == 1 {
            return Self { threads: 1, buf_size, depth: 0 };
        }
        // Input and output pipelines share the budget, and each needs at least
        // two buffers to overlap at all. Shrink the buffer before giving up, and
        // fall back to single-threaded rather than exceeding the budget.
        const MIN_BUF: usize = 32 * 1024;
        let per_stage = budget / 2;
        let mut buf_size = buf_size.max(1);
        while buf_size > MIN_BUF && per_stage / buf_size < 2 {
            buf_size /= 2;
        }
        let depth = per_stage / buf_size;
        if depth < 2 {
            return Self { threads: 1, buf_size, depth: 0 };
        }
        Self { threads, buf_size, depth: depth.min(8) }
    }

    pub fn is_threaded(&self) -> bool { self.threads > 1 && self.depth > 0 }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self::from_budget(default_threads(), 64 << 20, 256 * 1024)
    }
}

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn read_fill(r: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

pub struct ThreadedReader {
    data: Receiver<std::io::Result<Vec<u8>>>,
    free: SyncSender<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
    eof: bool,
    handle: Option<JoinHandle<()>>,
}

impl ThreadedReader {
    pub fn new(mut inner: Box<dyn Read + Send>, cfg: PipelineConfig) -> Self {
        let (data_tx, data) = sync_channel::<std::io::Result<Vec<u8>>>(cfg.depth);
        let (free, free_rx) = sync_channel::<Vec<u8>>(cfg.depth + 1);
        for _ in 0..cfg.depth {
            let _ = free.send(vec![0u8; cfg.buf_size]);
        }
        let buf_size = cfg.buf_size;
        let handle = std::thread::spawn(move || {
            while let Ok(mut buf) = free_rx.recv() {
                if buf.len() < buf_size {
                    buf.resize(buf_size, 0);
                }
                match read_fill(&mut *inner, &mut buf) {
                    Ok(0) => {
                        let _ = data_tx.send(Ok(Vec::new()));
                        break;
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        if data_tx.send(Ok(buf)).is_err() { break; }
                    }
                    Err(e) => {
                        let _ = data_tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        Self { data, free, cur: Vec::new(), pos: 0, eof: false, handle: Some(handle) }
    }
}

impl Read for ThreadedReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos == self.cur.len() {
            if self.eof {
                return Ok(0);
            }
            let done = std::mem::take(&mut self.cur);
            if !done.is_empty() {
                let _ = self.free.try_send(done);
            }
            match self.data.recv() {
                Ok(Ok(buf)) => {
                    if buf.is_empty() {
                        self.eof = true;
                        return Ok(0);
                    }
                    self.cur = buf;
                    self.pos = 0;
                }
                Ok(Err(e)) => {
                    self.eof = true;
                    return Err(e);
                }
                Err(_) => {
                    self.eof = true;
                    return Ok(0);
                }
            }
        }
        let n = (self.cur.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Drop for ThreadedReader {
    fn drop(&mut self) {
        // Dropping the free sender lets the producer observe a closed channel.
        let (dead, _) = sync_channel(1);
        let _ = std::mem::replace(&mut self.free, dead);
        while self.data.recv().is_ok() {}
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct ThreadedWriter {
    data: Option<SyncSender<Vec<u8>>>,
    free: Receiver<Vec<u8>>,
    cur: Vec<u8>,
    buf_size: usize,
    handle: Option<JoinHandle<std::io::Result<()>>>,
    failed: bool,
}

impl ThreadedWriter {
    pub fn new(mut inner: Box<dyn Write + Send>, cfg: PipelineConfig) -> Self {
        let (data, data_rx) = sync_channel::<Vec<u8>>(cfg.depth);
        let (free_tx, free) = sync_channel::<Vec<u8>>(cfg.depth + 1);
        let handle = std::thread::spawn(move || -> std::io::Result<()> {
            while let Ok(buf) = data_rx.recv() {
                inner.write_all(&buf)?;
                // try_send: a full free pool means the consumer is allocating
                // its own buffers, so dropping this one is correct. A blocking
                // send here deadlocks against the bounded data channel.
                let _ = free_tx.try_send(buf);
            }
            inner.flush()
        });
        Self {
            data: Some(data),
            free,
            cur: Vec::with_capacity(cfg.buf_size),
            buf_size: cfg.buf_size,
            handle: Some(handle),
            failed: false,
        }
    }

    fn take_buffer(&mut self) -> Vec<u8> {
        match self.free.try_recv() {
            Ok(mut b) => { b.clear(); b }
            Err(_) => Vec::with_capacity(self.buf_size),
        }
    }

    fn push(&mut self) -> std::io::Result<()> {
        if self.cur.is_empty() {
            return Ok(());
        }
        let next = self.take_buffer();
        let full = std::mem::replace(&mut self.cur, next);
        match self.data.as_ref().unwrap().send(full) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.failed = true;
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "writer thread stopped"))
            }
        }
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.push()?;
        drop(self.data.take());
        match self.handle.take() {
            Some(h) => h.join().map_err(|_| std::io::Error::other("writer thread panicked"))?,
            None => Ok(()),
        }
    }
}

impl Write for ThreadedWriter {
    fn write(&mut self, mut data: &[u8]) -> std::io::Result<usize> {
        let total = data.len();
        while !data.is_empty() {
            let space = self.buf_size.saturating_sub(self.cur.len()).max(1);
            let n = space.min(data.len());
            self.cur.extend_from_slice(&data[..n]);
            data = &data[n..];
            if self.cur.len() >= self.buf_size {
                self.push()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.push()
    }
}

impl Drop for ThreadedWriter {
    fn drop(&mut self) {
        drop(self.data.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PatchOptions {
    pub threads: usize,
    pub memory_budget: usize,
}

impl Default for PatchOptions {
    /// Single-threaded by default, because the benefit is workload-dependent
    /// while the memory cost is not.
    ///
    /// With `threads > 1` the diff stream is read ahead on one thread, output is
    /// written on another, and HDIFFW26 prefetches upcoming old-data windows
    /// (`WindowPrefetcher`). Measured:
    ///
    /// * 3 GiB, 1539 windows, page-cache resident: 2.7 s -> 2.3 s (~15% faster),
    ///   6.1 MiB -> 14.5 MiB.
    /// * 10 GiB, I/O-bound: no gain (18.6 s vs 19.6 s), 12.6 MiB -> 38.9 MiB.
    /// * 1 GiB decompression-bound lzma: no gain (8.2 s vs 8.4 s). A diff is one
    ///   sequential compressed stream, so the decode itself cannot be split.
    ///
    /// Opt in with `with_threads(n)` when a profile shows it helps; raise or
    /// lower `with_memory_budget` to control how many windows stay in flight.
    fn default() -> Self {
        Self { threads: 1, memory_budget: 64 << 20 }
    }
}

impl PatchOptions {
    pub fn single_threaded() -> Self {
        Self { threads: 1, memory_budget: 8 << 20 }
    }

    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    pub fn with_memory_budget(mut self, bytes: usize) -> Self {
        self.memory_budget = bytes;
        self
    }

    pub fn pipeline(&self, buf_size: usize) -> PipelineConfig {
        PipelineConfig::from_budget(self.threads, self.memory_budget, buf_size)
    }
}

/// Reads upcoming HDIFFW26 old-data windows on a background thread.
///
/// Mirrors `hcache_window_old_mt`: the main loop submits a batch of window
/// extents as soon as it has parsed the metadata, then collects them in order.
/// Peak memory is `depth * max_window_size`, and the request queue is sized to
/// hold a whole metadata batch so submitting can never block the consumer that
/// recycles the buffers.
pub struct WindowPrefetcher {
    req: Option<SyncSender<(u64, usize)>>,
    done: Receiver<std::io::Result<Vec<u8>>>,
    free: SyncSender<Vec<u8>>,
    handle: Option<JoinHandle<()>>,
}

impl WindowPrefetcher {
    /// Returns `None` when the budget cannot fund at least two windows in
    /// flight, in which case the caller keeps the serial read path.
    pub fn new(path: &std::path::Path, max_window_size: usize, budget: usize, req_capacity: usize) -> Option<Self> {
        if max_window_size == 0 {
            return None;
        }
        let depth = (budget / max_window_size).min(4);
        if depth < 2 {
            return None;
        }
        let mut file = std::fs::File::open(path).ok()?;

        let (req_tx, req_rx) = sync_channel::<(u64, usize)>(req_capacity.max(2));
        let (done_tx, done) = sync_channel::<std::io::Result<Vec<u8>>>(depth);
        let (free, free_rx) = sync_channel::<Vec<u8>>(depth + 1);
        for _ in 0..depth {
            let _ = free.send(vec![0u8; max_window_size]);
        }

        let handle = std::thread::spawn(move || {
            use std::io::{Read, Seek, SeekFrom};
            while let Ok((pos, len)) = req_rx.recv() {
                let mut buf = match free_rx.recv() {
                    Ok(b) => b,
                    Err(_) => break,
                };
                if buf.len() < len {
                    buf.resize(len, 0);
                }
                let result = file
                    .seek(SeekFrom::Start(pos))
                    .and_then(|_| file.read_exact(&mut buf[..len]))
                    .map(|_| buf);
                let failed = result.is_err();
                if done_tx.send(result).is_err() || failed {
                    break;
                }
            }
        });

        Some(Self { req: Some(req_tx), done, free, handle: Some(handle) })
    }

    pub fn submit(&self, pos: u64, len: usize) {
        if let Some(req) = self.req.as_ref() {
            let _ = req.send((pos, len));
        }
    }

    pub fn next(&mut self) -> std::io::Result<Vec<u8>> {
        match self.done.recv() {
            Ok(r) => r,
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "old-window prefetch thread stopped")),
        }
    }

    pub fn recycle(&self, buf: Vec<u8>) {
        let _ = self.free.try_send(buf);
    }
}

impl Drop for WindowPrefetcher {
    fn drop(&mut self) {
        drop(self.req.take());
        while self.done.recv().is_ok() {}
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
