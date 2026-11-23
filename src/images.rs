//! The images shown in the document: local files and http(s) URLs.
//!
//! Images are read, downloaded and decoded on worker threads, so a document
//! with many or large images opens at once and typing never waits for one.
//! An image is only loaded once its block is in view, and until it's ready
//! the block shows a placeholder. The newest requests are served first, so
//! after a fast scroll the blocks in view load before the ones scrolled past.
//! Large images are scaled to what the document column can show while
//! they're decoded, which keeps decoding fast and memory use low. Decoded
//! images are kept up to a memory budget, the ones shown longest ago going
//! first, and until another document is opened.
//!
//! Decoders, chosen for speed:
//! - JPEG: zune-jpeg (through `image`), or `jpeg-decoder` for photos at least
//!   twice as wide as shown: it decodes at 1/2, 1/4 or 1/8 size directly.
//! - WebP: libwebp, which also scales while decoding.
//! - PNG, GIF, BMP, ICO, TIFF, TGA, PNM, QOI: `image`.
//! - SVG: Slint (resvg), which renders it at the size it's shown at.
//!
//! Animated GIF and WebP show their first frame.

use fast_image_resize as fr;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, RgbImage, RgbaImage};
use slint::{Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

/// Largest download accepted.
const MAX_DOWNLOAD: u64 = 64 << 20;
/// Largest SVG shown, uncompressed: Slint parses it on the UI thread.
const MAX_SVG: usize = 8 << 20;
/// Largest image decoded, in pixels.
const MAX_PIXELS: u64 = 200_000_000;
/// Longest side of a decoded image: GPUs can't hold larger textures.
const MAX_SIDE: u32 = 16384;
/// Decoded images kept, in bytes: some 40 photos at the column's width.
const BUDGET: usize = 256 << 20;
/// Downloads at a time for the open document, as browsers allow per host.
const DOWNLOADS: usize = 6;
/// A worker that has had nothing to do for this long ends.
const IDLE_EXIT: Duration = Duration::from_secs(30);
/// How often finished images are collected while some are loading.
const POLL: Duration = Duration::from_millis(16);
/// AVIF is left out, so that servers that pick a format by this header
/// don't send it.
const ACCEPT: &str = "image/webp,image/png,image/jpeg,image/gif,image/svg+xml,image/*;q=0.8,*/*;q=0.5";
/// Why a load for a document that was closed stopped.
const CLOSED: &str = "the document was closed";

pub enum Lookup {
    Ready(slint::Image),
    /// Loading, or not asked for yet.
    Loading,
    Failed(String),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Key {
    /// A local file as it was when looked up: a changed file is read again.
    File { path: PathBuf, modified: Option<SystemTime>, len: u64 },
    Url(String),
}

impl Key {
    fn name(&self) -> String {
        match self {
            Key::File { path, .. } => path.display().to_string(),
            Key::Url(url) => url.clone(),
        }
    }
}

/// `used` is when the image was last asked for, for the memory budget.
enum Entry {
    Loading { used: u64 },
    /// Decoded for at most `width` pixels wide; `scaled` if it was scaled
    /// down to that. `bytes` is the memory it takes.
    Ready { image: slint::Image, width: u32, scaled: bool, bytes: usize, used: u64 },
    /// Being decoded again, wider: shown as it is meanwhile.
    Widening { image: slint::Image, bytes: usize, used: u64 },
    Failed(String),
}

impl Entry {
    fn touch(&mut self, now: u64) {
        match self {
            Entry::Loading { used } | Entry::Ready { used, .. } | Entry::Widening { used, .. } => *used = now,
            Entry::Failed(_) => {}
        }
    }
}

struct Job {
    generation: u64,
    key: Key,
    max_width: u32,
}

struct Done {
    generation: u64,
    key: Key,
    max_width: u32,
    result: Result<Decoded, String>,
}

/// What `Images::collect` changed in the cache.
#[derive(Default)]
pub struct Collected {
    /// Images, or why they failed, for blocks that waited for them.
    pub new: bool,
    /// Sharper copies of images already shown.
    pub sharper: bool,
    /// Images dropped for the memory budget: the blocks that show them
    /// should let go of them too.
    pub evicted: bool,
}

/// Decoded image data, which unlike `slint::Image` can be sent between threads.
#[derive(Debug)]
pub enum Decoded {
    Rgb(SharedPixelBuffer<Rgb8Pixel>),
    Rgba(SharedPixelBuffer<Rgba8Pixel>),
    /// SVG source: Slint renders it at the size it's shown at.
    Svg(Vec<u8>),
}

impl Decoded {
    /// The image, whether it was scaled down to `max_width`, the width it
    /// was decoded for, and about how much memory it takes.
    fn into_image(self, max_width: u32) -> Result<(slint::Image, bool, usize), String> {
        let pixels = |w: u32, h: u32| w as usize * h as usize;
        Ok(match self {
            Decoded::Rgb(b) => {
                let (scaled, bytes) = (b.width() >= max_width, pixels(b.width(), b.height()) * 3);
                (slint::Image::from_rgb8(b), scaled, bytes)
            }
            Decoded::Rgba(b) => {
                let (scaled, bytes) = (b.width() >= max_width, pixels(b.width(), b.height()) * 4);
                (slint::Image::from_rgba8(b), scaled, bytes)
            }
            // Rendered at the size it's shown at, at most `max_width` wide.
            Decoded::Svg(data) => {
                let image = slint::Image::load_from_svg_data(&data).map_err(|_| "not a valid SVG image".to_owned())?;
                let size = image.size();
                let w = size.width.clamp(1, max_width);
                let h = (size.height as u64 * w as u64 / size.width.max(1) as u64).min(MAX_SIDE as u64) as u32;
                (image, false, data.len() + pixels(w, h) * 4)
            }
        })
    }
}

/// What the UI thread and the workers share.
#[derive(Default)]
struct Shared {
    /// Changes when the document does; images of an older one are dropped.
    generation: AtomicU64,
    done: Mutex<Vec<Done>>,
}

impl Shared {
    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Relaxed) == generation
    }
}

/// The images of the open document. Lives on the UI thread.
pub struct Images {
    cache: RefCell<HashMap<Key, Entry>>,
    /// Memory taken by the images in `cache`.
    bytes: Cell<usize>,
    budget: usize,
    /// Counts requests, to tell which image was asked for longest ago.
    clock: Cell<u64>,
    shared: Arc<Shared>,
    loading: Cell<usize>,
    files: OnceCell<Arc<Pool>>,
    downloads: OnceCell<Arc<Pool>>,
    timer: slint::Timer,
    on_ready: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Images {
    pub fn new() -> Self {
        Self::with_budget(BUDGET)
    }

    pub(crate) fn with_budget(budget: usize) -> Self {
        Images {
            cache: RefCell::default(),
            bytes: Cell::new(0),
            budget,
            clock: Cell::new(0),
            shared: Arc::default(),
            loading: Cell::new(0),
            files: OnceCell::new(),
            downloads: OnceCell::new(),
            timer: slint::Timer::default(),
            on_ready: RefCell::default(),
        }
    }

    /// How many images were asked for, loaded or not.
    #[cfg(test)]
    pub fn requested(&self) -> usize {
        self.cache.borrow().len()
    }

    /// Called while images are loading, when some may have arrived; it should
    /// call `collect` and show the ones that did.
    pub fn set_on_ready(&self, f: impl Fn() + 'static) {
        *self.on_ready.borrow_mut() = Some(Rc::new(f));
        self.poll();
    }

    /// The image for `url`, relative to `base`, if it's ready. Doesn't load
    /// it: see `request`.
    pub fn get(&self, url: &str, base: &Path) -> Lookup {
        let key = match resolve(url, base) {
            Ok(key) => key,
            Err(why) => return Lookup::Failed(why),
        };
        match self.cache.borrow().get(&key) {
            Some(Entry::Ready { image, .. } | Entry::Widening { image, .. }) => Lookup::Ready(image.clone()),
            Some(Entry::Failed(why)) => Lookup::Failed(why.clone()),
            Some(Entry::Loading { .. }) | None => Lookup::Loading,
        }
    }

    /// The block showing `url` is in view: loads its image, at most
    /// `max_width` pixels wide, unless it's loaded or loading. One that was
    /// scaled down to a narrower width is decoded again (the window is on a
    /// screen with a larger scale now) and shown as it is meanwhile.
    pub fn request(&self, url: &str, base: &Path, max_width: u32) {
        let Ok(key) = resolve(url, base) else { return };
        let now = self.clock.get() + 1;
        self.clock.set(now);
        let mut cache = self.cache.borrow_mut();
        let entry = match cache.get_mut(&key) {
            Some(Entry::Ready { image, width, scaled: true, bytes, .. }) if max_width > *width => {
                Entry::Widening { image: image.clone(), bytes: *bytes, used: now }
            }
            Some(entry) => {
                entry.touch(now);
                return;
            }
            None => Entry::Loading { used: now },
        };
        let entry = if self.start(key.clone(), max_width) {
            entry
        } else {
            match entry {
                // Keep showing it.
                Entry::Widening { image, bytes, used } => Entry::Ready { image, width: max_width, scaled: false, bytes, used },
                _ => Entry::Failed("can't start loading images".into()),
            }
        };
        cache.insert(key, entry);
    }

    /// Queues loading `key`. Returns false if there's no thread to load it.
    fn start(&self, key: Key, max_width: u32) -> bool {
        let remote = matches!(key, Key::Url(_));
        let job = Job { generation: self.shared.generation.load(Ordering::Relaxed), key, max_width };
        let pool = if remote { &self.downloads } else { &self.files };
        let pool = pool.get_or_init(|| {
            let threads = if remote {
                DOWNLOADS
            } else {
                // Leave a core for the UI.
                std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1)).clamp(1, 8)
            };
            Pool::new(threads, remote)
        });
        if !pool.push(job, &self.shared) {
            return false;
        }
        self.loading.set(self.loading.get() + 1);
        self.poll();
        true
    }

    /// Moves the images that arrived into the cache, then keeps it within
    /// the memory budget.
    pub fn collect(&self) -> Collected {
        let done = std::mem::take(&mut *self.shared.done.lock().unwrap());
        let mut collected = Collected::default();
        let mut cache = self.cache.borrow_mut();
        let mut bytes = self.bytes.get();
        for d in done.into_iter().filter(|d| self.shared.is_current(d.generation)) {
            self.loading.set(self.loading.get().saturating_sub(1));
            let (widening, used) = match cache.remove(&d.key) {
                Some(Entry::Widening { image, bytes: old, used }) => {
                    bytes -= old;
                    (Some((image, old)), used)
                }
                Some(Entry::Ready { bytes: old, used, .. }) => {
                    bytes -= old;
                    (None, used)
                }
                Some(Entry::Loading { used }) => (None, used),
                Some(Entry::Failed(_)) | None => (None, self.clock.get()),
            };
            let entry = match (d.result.and_then(|decoded| decoded.into_image(d.max_width)), widening) {
                (Ok((image, scaled, size)), widening) => {
                    collected.sharper |= widening.is_some();
                    collected.new |= widening.is_none();
                    bytes += size;
                    Entry::Ready { image, width: d.max_width, scaled, bytes: size, used }
                }
                // Keep showing the image it already has, and don't try again.
                (Err(_), Some((image, old))) => {
                    bytes += old;
                    Entry::Ready { image, width: d.max_width, scaled: false, bytes: old, used }
                }
                (Err(why), None) => {
                    collected.new = true;
                    Entry::Failed(why)
                }
            };
            cache.insert(d.key, entry);
        }
        // The images asked for longest ago go first, but not the last one.
        while bytes > self.budget {
            let ready = cache.iter().filter_map(|(key, e)| match e {
                Entry::Ready { used, bytes, .. } => Some((*used, *bytes, key)),
                _ => None,
            });
            let (count, oldest) = ready.fold((0, None), |(n, oldest), r| {
                (n + 1, if oldest.is_none_or(|o: (u64, usize, &Key)| r.0 < o.0) { Some(r) } else { oldest })
            });
            let Some((_, size, key)) = oldest.filter(|_| count > 1) else { break };
            let key = key.clone();
            cache.remove(&key);
            bytes -= size;
            collected.evicted = true;
        }
        self.bytes.set(bytes);
        if self.loading.get() == 0 {
            self.timer.stop();
        }
        collected
    }

    /// Forgets all images, for another document. Images still loading are
    /// dropped when they arrive, and don't hold up the new document's.
    pub fn clear(&self) {
        let generation = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
        for pool in [self.files.get(), self.downloads.get()].into_iter().flatten() {
            pool.clear(generation);
        }
        // After the generation changes, so that workers add no more of them.
        self.shared.done.lock().unwrap().clear();
        self.cache.borrow_mut().clear();
        self.bytes.set(0);
        self.loading.set(0);
        self.timer.stop();
    }

    fn poll(&self) {
        if self.loading.get() == 0 || self.timer.running() {
            return;
        }
        if let Some(f) = self.on_ready.borrow().clone() {
            self.timer.start(slint::TimerMode::Repeated, POLL, move || f());
        }
    }
}

impl Drop for Images {
    fn drop(&mut self) {
        for pool in [self.files.get(), self.downloads.get()].into_iter().flatten() {
            pool.close();
        }
    }
}

/// Worker threads that load images, the newest request first. They're
/// started as needed, up to `limit` for the open document: workers still
/// busy with an image of a document that was closed, such as a download
/// that hangs, don't count, so they don't hold up the next document.
struct Pool {
    queue: Mutex<Queue>,
    work: Condvar,
    limit: usize,
    remote: bool,
    agent: Option<ureq::Agent>,
}

#[derive(Default)]
struct Queue {
    /// Newest last.
    jobs: Vec<Job>,
    /// The open document's generation.
    generation: u64,
    /// Workers waiting for a job.
    idle: usize,
    /// Workers loading an image for the open document.
    busy: usize,
    threads: usize,
    closed: bool,
}

impl Pool {
    fn new(limit: usize, remote: bool) -> Arc<Pool> {
        Arc::new(Pool { queue: Mutex::default(), work: Condvar::new(), limit, remote, agent: remote.then(agent) })
    }

    /// Queues `job`, and starts a worker for it if there's none free and
    /// room for one. Returns false if there's no worker to run it.
    fn push(self: &Arc<Self>, job: Job, shared: &Arc<Shared>) -> bool {
        let mut q = self.queue.lock().unwrap();
        q.jobs.push(job);
        // Workers busy with another document's images, up to as many again
        // as the limit (three documents opened while their downloads hang)
        // before new jobs wait for them.
        if q.jobs.len() > q.idle && q.idle + q.busy < self.limit && q.threads < self.limit * 4 {
            let (pool, shared) = (self.clone(), shared.clone());
            let name = format!("{}-{}", if self.remote { "download" } else { "image" }, q.threads);
            match std::thread::Builder::new().name(name).spawn(move || pool.run(&shared)) {
                Ok(_) => {
                    q.idle += 1;
                    q.threads += 1;
                }
                Err(_) if q.threads == 0 => {
                    q.jobs.pop();
                    return false;
                }
                Err(_) => {}
            }
        }
        self.work.notify_one();
        true
    }

    /// Another document: drops the jobs of the old one, and frees the places
    /// of the workers busy with its images.
    fn clear(&self, generation: u64) {
        let mut q = self.queue.lock().unwrap();
        q.jobs.clear();
        q.generation = generation;
        q.busy = 0;
    }

    /// Ends the workers, once they finish what they're doing.
    fn close(&self) {
        let mut q = self.queue.lock().unwrap();
        q.closed = true;
        q.jobs.clear();
        self.work.notify_all();
    }

    /// A worker. It's counted as idle when started.
    fn run(&self, shared: &Shared) {
        let mut q = self.queue.lock().unwrap();
        loop {
            if q.closed {
                break;
            }
            let Some(job) = q.jobs.pop() else {
                let (guard, wait) = self.work.wait_timeout(q, IDLE_EXIT).unwrap();
                q = guard;
                if wait.timed_out() && q.jobs.is_empty() {
                    break;
                }
                continue;
            };
            q.idle -= 1;
            q.busy += 1;
            drop(q);
            let stale = || !shared.is_current(job.generation);
            // A decoder bug on a bad file shouldn't leave the image loading forever.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(&job.key, job.max_width, self.agent.as_ref(), &stale)))
                .unwrap_or_else(|_| Err(format!("{}: the decoder failed", job.key.name())));
            {
                // The document may have changed meanwhile: then `clear` has
                // emptied `done`, and nothing would take the image out again.
                let mut done = shared.done.lock().unwrap();
                if shared.is_current(job.generation) {
                    done.push(Done { generation: job.generation, key: job.key, max_width: job.max_width, result });
                }
            }
            q = self.queue.lock().unwrap();
            if q.generation == job.generation {
                q.busy -= 1;
            }
            q.idle += 1;
        }
        q.idle -= 1;
        q.threads -= 1;
    }
}

/// Turns an image URL from the document into a cache key.
fn resolve(url: &str, base: &Path) -> Result<Key, String> {
    let unsupported = || format!("unsupported link ({})", url.chars().take(60).collect::<String>());
    let local = match url.split_once("://") {
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") => {
            return Ok(Key::Url(url.to_owned()));
        }
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("file") => file_url_path(rest).ok_or_else(unsupported)?,
        None if !url.starts_with("data:") => url.to_owned(),
        _ => return Err(unsupported()),
    };
    let path = PathBuf::from(crate::percent_decode(&local));
    let path = if path.is_absolute() { path } else { base.join(path) };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(Key::File { modified: meta.modified().ok(), len: meta.len(), path }),
        Ok(_) => Err(format!("can't load {}: not a file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("can't load {}: file not found", path.display())),
        Err(e) => Err(format!("can't load {}: {e}", path.display())),
    }
}

/// The path of a file URL, given what follows `file://`: `/home/a.png`,
/// `localhost/home/a.png`, `/C:/notes/a.png`, or on Windows a share,
/// `server/share/a.png`. Still percent-encoded.
fn file_url_path(rest: &str) -> Option<String> {
    let (host, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, ""),
    };
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        // /C:/notes/a.png
        let b = path.as_bytes();
        let drive = b.len() > 3 && b[1].is_ascii_alphabetic() && b[2] == b':';
        return Some(if drive { &path[1..] } else { path }.to_owned());
    }
    cfg!(windows).then(|| format!("//{rest}"))
}

/// Runs on a worker thread. Gives up once `stale` says the document was closed.
fn load(key: &Key, max_width: u32, agent: Option<&ureq::Agent>, stale: &dyn Fn() -> bool) -> Result<Decoded, String> {
    if stale() {
        return Err(CLOSED.into());
    }
    let (bytes, ext) = match key {
        Key::File { path, .. } => {
            let bytes = std::fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
            (bytes, path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()))
        }
        Key::Url(url) => {
            let agent = agent.ok_or("no downloader")?;
            let (bytes, content_type) = fetch(agent, url, stale).map_err(|why| format!("can't download {url}: {why}"))?;
            let ext = match content_type.as_deref() {
                Some(t) if t.starts_with("image/svg") => Some("svg".to_owned()),
                _ => url_extension(url),
            };
            (bytes, ext)
        }
    };
    if stale() {
        return Err(CLOSED.into());
    }
    decode(&bytes, ext.as_deref(), max_width).map_err(|why| format!("{}: {why}", key.name()))
}

fn url_extension(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

// ----- downloads ------------------------------------------------------------------------

fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // The system's TLS with the system's certificates, and the proxy from
    // HTTPS_PROXY and friends or, on Windows, the system setting.
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(concat!("linter/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The body and content type of `url`. Stops reading once `stale` says the
/// document was closed.
fn fetch(agent: &ureq::Agent, url: &str, stale: &dyn Fn() -> bool) -> Result<(Vec<u8>, Option<String>), String> {
    use std::io::Read;
    let mut request = agent.get(url).header("Accept", ACCEPT);
    // A proxy can't reach this machine's own servers, so browsers bypass it
    // for them; ureq only does if NO_PROXY says so.
    if request.uri_ref().and_then(|u| u.host()).is_some_and(is_loopback) {
        request = request.config().proxy(None).build();
    }
    let mut response = request.call().map_err(|e| describe(&e))?;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase());
    let mut body = response.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let (mut bytes, mut chunk) = (Vec::new(), vec![0; 64 << 10]);
    loop {
        if stale() {
            return Err(CLOSED.into());
        }
        match body.read(&mut chunk) {
            Ok(0) => return Ok((bytes, content_type)),
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(describe(&ureq::Error::from(e))),
        }
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn describe(e: &ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code) => {
            let reason = ureq::http::StatusCode::from_u16(*code).ok().and_then(|s| s.canonical_reason());
            format!("the server answered {code}{}", reason.map(|r| format!(" {r}")).unwrap_or_default())
        }
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::Timeout(_) => "timed out".into(),
        ureq::Error::BodyExceedsLimit(_) => format!("larger than {} MB", MAX_DOWNLOAD >> 20),
        ureq::Error::Io(e) => e.to_string(),
        e => {
            let text = e.to_string();
            match text.strip_prefix("native-tls: ") {
                // OpenSSL puts the reason last, in parentheses: "(certificate has expired)".
                Some(tls) => {
                    let reason = tls.rsplit_once('(').and_then(|(_, r)| r.strip_suffix(')')).unwrap_or(tls);
                    format!("secure connection failed: {}", reason.trim())
                }
                None => text,
            }
        }
    }
}

// ----- decoding -------------------------------------------------------------------------

enum Format {
    Jpeg,
    WebP,
    Svg,
    Other(ImageFormat),
}

/// Decodes an image file, scaled to at most `max_width` pixels wide.
/// `ext` is the file's extension, for formats without a signature.
pub fn decode(bytes: &[u8], ext: Option<&str>, max_width: u32) -> Result<Decoded, String> {
    match sniff(bytes, ext)? {
        Format::Svg => svg(bytes),
        Format::Jpeg => jpeg(bytes, max_width),
        Format::WebP => webp(bytes, max_width),
        Format::Other(format) => decode_with(bytes, format, max_width),
    }
}

fn sniff(bytes: &[u8], ext: Option<&str>) -> Result<Format, String> {
    // ISO media files: the brand says what's inside.
    if bytes.get(4..8) == Some(b"ftyp") {
        match bytes.get(8..12) {
            Some(b"avif" | b"avis") => return Err("AVIF images aren't supported".into()),
            Some(b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"mif1" | b"msf1") => {
                return Err("HEIC images aren't supported".into());
            }
            _ => {}
        }
    }
    if bytes.starts_with(&[0xff, 0x0a]) || bytes.starts_with(b"\0\0\0\x0cJXL ") {
        return Err("JPEG XL images aren't supported".into());
    }
    match image::guess_format(bytes) {
        Ok(ImageFormat::Jpeg) => return Ok(Format::Jpeg),
        Ok(ImageFormat::WebP) => return Ok(Format::WebP),
        Ok(format) if format.reading_enabled() => return Ok(Format::Other(format)),
        Ok(format) => return Err(format!("{format:?} images aren't supported")),
        Err(_) => {}
    }
    if is_svg(bytes, ext) {
        return Ok(Format::Svg);
    }
    // TGA has no signature.
    if ext.and_then(ImageFormat::from_extension) == Some(ImageFormat::Tga) {
        return Ok(Format::Other(ImageFormat::Tga));
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    Err(if head.contains("<html") || head.contains("<!doctype html") {
        "not an image (got a web page)".into()
    } else {
        "not a supported image format".into()
    })
}

fn is_svg(bytes: &[u8], ext: Option<&str>) -> bool {
    if matches!(ext, Some("svg" | "svgz")) {
        return true;
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('<') && head.contains("<svg")
}

/// SVG source for Slint, which parses it on the UI thread: unpacked here if
/// it's compressed (.svgz), and only if it's small enough to parse quickly.
fn svg(bytes: &[u8]) -> Result<Decoded, String> {
    use std::io::Read;
    let too_large = || format!("too large to show (an SVG image over {} MB)", MAX_SVG >> 20);
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return if bytes.len() > MAX_SVG { Err(too_large()) } else { Ok(Decoded::Svg(bytes.to_vec())) };
    }
    let mut source = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_SVG as u64 + 1)
        .read_to_end(&mut source)
        .map_err(|_| "not a valid compressed SVG image".to_owned())?;
    if source.len() > MAX_SVG { Err(too_large()) } else { Ok(Decoded::Svg(source)) }
}

/// The size to decode a `w` × `h` image at: at most `max_width` wide once
/// `orientation` is applied, and at most MAX_SIDE on either side.
fn fit(w: u32, h: u32, orientation: Orientation, max_width: u32) -> (u32, u32) {
    let shown_w = if turns(orientation) { h } else { w };
    let scale = (max_width as f64 / shown_w as f64).min(MAX_SIDE as f64 / w.max(h) as f64);
    if scale >= 1.0 {
        return (w, h);
    }
    let size = |n: u32| ((n as f64 * scale).round() as u32).max(1);
    (size(w), size(h))
}

fn turns(orientation: Orientation) -> bool {
    use Orientation::*;
    matches!(orientation, Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH)
}

fn check_size(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 {
        Err("the image is empty".into())
    } else if w as u64 * h as u64 > MAX_PIXELS {
        Err(format!("too large to show ({w} × {h})"))
    } else {
        Ok(())
    }
}

fn jpeg(bytes: &[u8], max_width: u32) -> Result<Decoded, String> {
    use jpeg_decoder::PixelFormat;
    let mut decoder = jpeg_decoder::Decoder::new(bytes);
    if decoder.read_info().is_ok()
        && let Some(info) = decoder.info()
    {
        let (w, h) = (info.width as u32, info.height as u32);
        check_size(w, h)?;
        let orientation = decoder.exif_data().and_then(Orientation::from_exif_chunk).unwrap_or(Orientation::NoTransforms);
        let (tw, th) = fit(w, h, orientation, max_width);
        // Decoding at 1/2, 1/4 or 1/8 size is about twice as fast as a full
        // decode with zune-jpeg, which is faster at full size.
        if tw * 2 <= w
            && matches!(info.pixel_format, PixelFormat::RGB24 | PixelFormat::L8)
            && let Ok((sw, sh)) = decoder.scale(tw as u16, th as u16)
            && let Ok(pixels) = decoder.decode()
        {
            let (sw, sh) = (sw as u32, sh as u32);
            let image = match info.pixel_format {
                PixelFormat::RGB24 => RgbImage::from_raw(sw, sh, pixels).map(DynamicImage::ImageRgb8),
                _ => image::GrayImage::from_raw(sw, sh, pixels).map(DynamicImage::ImageLuma8),
            };
            if let Some(image) = image {
                return finish(image, orientation, max_width);
            }
        }
    }
    decode_with(bytes, ImageFormat::Jpeg, max_width)
}

fn decode_with(bytes: &[u8], format: ImageFormat, max_width: u32) -> Result<Decoded, String> {
    let mut decoder = image::ImageReader::with_format(Cursor::new(bytes), format).into_decoder().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions();
    check_size(w, h)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    finish(image, orientation, max_width)
}

/// Scales, turns upright and converts a decoded image.
fn finish(image: DynamicImage, orientation: Orientation, max_width: u32) -> Result<Decoded, String> {
    let mut image = match image {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => image,
        _ if image.color().has_alpha() => DynamicImage::ImageRgba8(image.into_rgba8()),
        _ => DynamicImage::ImageRgb8(image.into_rgb8()),
    };
    let (w, h) = (image.width(), image.height());
    let (tw, th) = fit(w, h, orientation, max_width);
    if (tw, th) != (w, h) {
        image = resize(image, tw, th)?;
    }
    image.apply_orientation(orientation);
    let (w, h) = (image.width(), image.height());
    Ok(match image {
        DynamicImage::ImageRgba8(b) => Decoded::Rgba(SharedPixelBuffer::clone_from_slice(b.as_raw(), w, h)),
        other => Decoded::Rgb(SharedPixelBuffer::clone_from_slice(other.into_rgb8().as_raw(), w, h)),
    })
}

fn resize(image: DynamicImage, w: u32, h: u32) -> Result<DynamicImage, String> {
    let rgba = matches!(image, DynamicImage::ImageRgba8(_));
    let pixel = if rgba { fr::PixelType::U8x4 } else { fr::PixelType::U8x3 };
    let src = fr::images::Image::from_vec_u8(image.width(), image.height(), image.into_bytes(), pixel).map_err(|e| e.to_string())?;
    let mut dst = fr::images::Image::new(w, h, pixel);
    // Catmull-Rom is sharp and a little faster than Lanczos. Alpha is
    // premultiplied while resizing, so edges don't darken.
    let options = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::CatmullRom));
    fr::Resizer::new().resize(&src, &mut dst, &options).map_err(|e| e.to_string())?;
    let bytes = dst.into_vec();
    let image = if rgba {
        RgbaImage::from_raw(w, h, bytes).map(DynamicImage::ImageRgba8)
    } else {
        RgbImage::from_raw(w, h, bytes).map(DynamicImage::ImageRgb8)
    };
    image.ok_or_else(|| "resize failed".into())
}

fn webp(bytes: &[u8], max_width: u32) -> Result<Decoded, String> {
    use libwebp_sys as w;
    use w::VP8StatusCode::VP8_STATUS_OK;
    let mut config = w::WebPDecoderConfig::new().map_err(|_| "libwebp doesn't match its headers")?;
    // SAFETY: libwebp reads `bytes` and writes `config`, and the output
    // buffer it's given below is exactly the size it's told.
    unsafe {
        if w::WebPGetFeatures(bytes.as_ptr(), bytes.len(), &mut config.input) != VP8_STATUS_OK {
            return Err("not a valid WebP image".into());
        }
        if config.input.has_animation != 0 {
            return webp_first_frame(bytes, max_width);
        }
        let (width, height) = (config.input.width as u32, config.input.height as u32);
        check_size(width, height)?;
        let (tw, th) = fit(width, height, Orientation::NoTransforms, max_width);
        if (tw, th) != (width, height) {
            config.options.use_scaling = 1;
            config.options.scaled_width = tw as i32;
            config.options.scaled_height = th as i32;
        }
        config.options.use_threads = 1;
        config.output.is_external_memory = 1;
        let alpha = config.input.has_alpha != 0;
        let mut decode_into = |out: &mut [u8], channels: u32, mode| {
            config.output.colorspace = mode;
            config.output.u.RGBA = w::WebPRGBABuffer { rgba: out.as_mut_ptr(), stride: (tw * channels) as i32, size: out.len() };
            let status = w::WebPDecode(bytes.as_ptr(), bytes.len(), &mut config);
            w::WebPFreeDecBuffer(&mut config.output);
            if status == VP8_STATUS_OK { Ok(()) } else { Err(format!("can't decode this WebP image ({status:?})")) }
        };
        if alpha {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(tw, th);
            decode_into(buffer.make_mut_bytes(), 4, w::WEBP_CSP_MODE::MODE_RGBA)?;
            Ok(Decoded::Rgba(buffer))
        } else {
            let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(tw, th);
            decode_into(buffer.make_mut_bytes(), 3, w::WEBP_CSP_MODE::MODE_RGB)?;
            Ok(Decoded::Rgb(buffer))
        }
    }
}

/// # Safety
/// `bytes` must be a WebP file (checked with WebPGetFeatures).
unsafe fn webp_first_frame(bytes: &[u8], max_width: u32) -> Result<Decoded, String> {
    use libwebp_sys as w;
    let failed = || "can't decode this animated WebP image".to_owned();
    // SAFETY: the decoder reads `data`, which outlives it, and the frame it
    // returns is canvas_width × canvas_height RGBA pixels, valid until the
    // decoder is deleted.
    let frame = unsafe {
        let mut options: w::WebPAnimDecoderOptions = std::mem::zeroed();
        if w::WebPAnimDecoderOptionsInit(&mut options) == 0 {
            return Err(failed());
        }
        options.color_mode = w::WEBP_CSP_MODE::MODE_RGBA;
        options.use_threads = 1;
        let data = w::WebPData { bytes: bytes.as_ptr(), size: bytes.len() };
        let decoder = w::WebPAnimDecoderNew(&data, &options);
        if decoder.is_null() {
            return Err(failed());
        }
        let mut info = w::WebPAnimInfo::default();
        let mut pixels = std::ptr::null_mut();
        let mut timestamp = 0;
        let frame = if w::WebPAnimDecoderGetInfo(decoder, &mut info) != 0
            && check_size(info.canvas_width, info.canvas_height).is_ok()
            && w::WebPAnimDecoderGetNext(decoder, &mut pixels, &mut timestamp) != 0
            && !pixels.is_null()
        {
            let len = info.canvas_width as usize * info.canvas_height as usize * 4;
            RgbaImage::from_raw(info.canvas_width, info.canvas_height, std::slice::from_raw_parts(pixels, len).to_vec())
        } else {
            None
        };
        w::WebPAnimDecoderDelete(decoder);
        frame
    };
    finish(DynamicImage::ImageRgba8(frame.ok_or_else(failed)?), Orientation::NoTransforms, max_width)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};

    pub(crate) fn gradient(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, 128]))
    }

    pub(crate) fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    pub(crate) fn webp_bytes(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let rgba = RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 90, if alpha { 128 } else { 255 }]));
        unsafe {
            let mut out = std::ptr::null_mut();
            let len = libwebp_sys::WebPEncodeRGBA(rgba.as_raw().as_ptr(), w as i32, h as i32, (w * 4) as i32, 80.0, &mut out);
            assert!(len > 0);
            let bytes = std::slice::from_raw_parts(out, len).to_vec();
            libwebp_sys::WebPFree(out as *mut _);
            bytes
        }
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut out = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        out.write_all(data).unwrap();
        out.finish().unwrap()
    }

    fn size(d: &Decoded) -> (u32, u32) {
        match d {
            Decoded::Rgb(b) => (b.width(), b.height()),
            Decoded::Rgba(b) => (b.width(), b.height()),
            Decoded::Svg(_) => (0, 0),
        }
    }

    /// A JPEG with an Exif orientation tag (6: shown turned 90° clockwise).
    fn jpeg_with_orientation(w: u32, h: u32, orientation: u16) -> Vec<u8> {
        let jpeg = encode(&DynamicImage::ImageRgb8(gradient(w, h)), ImageFormat::Jpeg);
        let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
        tiff.extend([0x12, 0x01, 3, 0, 1, 0, 0, 0]);
        tiff.extend(orientation.to_le_bytes());
        tiff.extend([0, 0, 0, 0, 0, 0]);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend([0xff, 0xe1]);
        out.extend(((app1.len() + 2) as u16).to_be_bytes());
        out.extend(app1);
        out.extend(&jpeg[2..]);
        out
    }

    #[test]
    fn decodes_each_format() {
        let rgb = DynamicImage::ImageRgb8(gradient(40, 30));
        let rgba = DynamicImage::ImageRgba8(DynamicImage::ImageRgb8(gradient(40, 30)).into_rgba8());
        for (format, image, ext) in [
            (ImageFormat::Png, &rgba, "png"),
            (ImageFormat::Jpeg, &rgb, "jpg"),
            (ImageFormat::Gif, &rgba, "gif"),
            (ImageFormat::Bmp, &rgb, "bmp"),
            (ImageFormat::Ico, &rgba, "ico"),
            (ImageFormat::Tiff, &rgb, "tiff"),
            (ImageFormat::Tga, &rgb, "tga"),
            (ImageFormat::Pnm, &rgb, "ppm"),
            (ImageFormat::Qoi, &rgba, "qoi"),
        ] {
            let bytes = encode(image, format);
            let decoded = decode(&bytes, Some(ext), 1640).unwrap_or_else(|e| panic!("{format:?}: {e}"));
            assert_eq!(size(&decoded), (40, 30), "{format:?}");
        }
        let decoded = decode(&webp_bytes(40, 30, false), None, 1640).unwrap();
        assert!(matches!(decoded, Decoded::Rgb(_)));
        assert_eq!(size(&decoded), (40, 30));
        let decoded = decode(&webp_bytes(40, 30, true), None, 1640).unwrap();
        assert!(matches!(decoded, Decoded::Rgba(_)), "WebP with alpha keeps it");
    }

    #[test]
    fn large_images_are_scaled_to_the_column() {
        let png = encode(&DynamicImage::ImageRgb8(gradient(4000, 100)), ImageFormat::Png);
        assert_eq!(size(&decode(&png, None, 1640).unwrap()), (1640, 41));
        // Big JPEGs are decoded at a smaller size, then resized.
        let jpeg = encode(&DynamicImage::ImageRgb8(gradient(2000, 1500)), ImageFormat::Jpeg);
        assert_eq!(size(&decode(&jpeg, None, 800).unwrap()), (800, 600));
        assert_eq!(size(&decode(&webp_bytes(2000, 1500, false), None, 800).unwrap()), (800, 600));
        // Small images are left as they are.
        assert_eq!(size(&decode(&jpeg, None, 5000).unwrap()), (2000, 1500));
        // Very tall images fit the GPU's largest texture.
        let tall = encode(&DynamicImage::ImageRgb8(gradient(100, 20000)), ImageFormat::Png);
        assert_eq!(size(&decode(&tall, None, 1640).unwrap()), (82, 16384));
    }

    #[test]
    fn jpeg_orientation_is_applied() {
        // Small: decoded in full by zune-jpeg.
        assert_eq!(size(&decode(&jpeg_with_orientation(60, 40, 6), None, 1640).unwrap()), (40, 60));
        assert_eq!(size(&decode(&jpeg_with_orientation(60, 40, 1), None, 1640).unwrap()), (60, 40));
        // Large: upright it's 1500 × 2000. For 820 wide it's decoded in full,
        // for 500 wide jpeg-decoder decodes it at half size.
        let jpeg = jpeg_with_orientation(2000, 1500, 6);
        assert_eq!(size(&decode(&jpeg, None, 820).unwrap()), (820, 1093));
        assert_eq!(size(&decode(&jpeg, None, 500).unwrap()), (500, 667));
    }

    #[test]
    fn recognises_svg_and_unsupported_files() {
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#;
        assert!(matches!(decode(svg, None, 1640), Ok(Decoded::Svg(_))));
        assert!(matches!(decode(b"\xef\xbb\xbf <svg/>", Some("xml"), 1640), Ok(Decoded::Svg(_))));
        // .svgz is unpacked for Slint.
        let svgz = gzip(svg);
        assert!(matches!(decode(&svgz, Some("svgz"), 1640), Ok(Decoded::Svg(s)) if s == svg));
        assert!(decode(b"\x1f\x8b\x08", Some("svgz"), 1640).is_err());
        // SVG too large to parse quickly, plain or packed.
        let mut huge = b"<svg xmlns=\"http://www.w3.org/2000/svg\">".to_vec();
        huge.resize(MAX_SVG + 1, b' ');
        assert!(decode(&huge, None, 1640).unwrap_err().starts_with("too large to show"));
        assert!(decode(&gzip(&huge), Some("svgz"), 1640).unwrap_err().starts_with("too large to show"));
        let avif = b"\0\0\0\x1cftypavif\0\0\0\0avifmif1";
        assert_eq!(decode(avif, Some("avif"), 1640).unwrap_err(), "AVIF images aren't supported");
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
        assert_eq!(decode(heic, None, 1640).unwrap_err(), "HEIC images aren't supported");
        let page = b"<!DOCTYPE html><html><body>Sign in</body></html>";
        assert_eq!(decode(page, Some("png"), 1640).unwrap_err(), "not an image (got a web page)");
        assert_eq!(decode(b"hello", None, 1640).unwrap_err(), "not a supported image format");
        assert!(decode(b"\x89PNG\r\n\x1a\n broken", None, 1640).is_err());
    }

    #[test]
    fn resolves_links() {
        let dir = std::env::temp_dir().join(format!("linter-images-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a b")).unwrap();
        std::fs::write(dir.join("a b/pic.png"), b"x").unwrap();
        assert!(matches!(resolve("https://example.com/a.png", &dir), Ok(Key::Url(_))));
        assert!(matches!(resolve("HTTP://example.com/a.png", &dir), Ok(Key::Url(_))));
        assert!(matches!(resolve("a%20b/pic.png", &dir), Ok(Key::File { len: 1, .. })));
        let url = format!("file://{}", dir.join("a b/pic.png").display());
        assert!(matches!(resolve(&url, &dir), Ok(Key::File { .. })));
        // The scheme in any case, and the host `localhost`.
        let url = format!("FILE://{}", dir.join("a b/pic.png").display());
        assert!(matches!(resolve(&url, &dir), Ok(Key::File { .. })), "{url}");
        let url = format!("file://localhost{}", dir.join("a b/pic.png").display());
        assert!(matches!(resolve(&url, &dir), Ok(Key::File { .. })), "{url}");
        assert_eq!(file_url_path("/C:/notes/a.png").as_deref(), Some("C:/notes/a.png"));
        assert_eq!(file_url_path("LocalHost/home/a.png").as_deref(), Some("/home/a.png"));
        if !cfg!(windows) {
            assert!(resolve("file://server/share/a.png", &dir).unwrap_err().starts_with("unsupported link"));
        }
        assert!(resolve("missing.png", &dir).unwrap_err().ends_with("missing.png: file not found"));
        assert!(resolve("a b", &dir).unwrap_err().ends_with("not a file"));
        assert!(resolve("data:image/png;base64,AAAA", &dir).unwrap_err().starts_with("unsupported link"));
        assert!(resolve("ftp://example.com/a.png", &dir).unwrap_err().starts_with("unsupported link"));
        assert_eq!(url_extension("https://x.org/a/b.SVG?v=2#top").as_deref(), Some("svg"));
        assert_eq!(url_extension("https://x.org/a/b"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serves each request with the next of `responses` (status, content type, body).
    pub(crate) fn serve(responses: Vec<(u16, &'static str, Vec<u8>)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, content_type, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn downloads_images() {
        let png = encode(&DynamicImage::ImageRgb8(gradient(40, 30)), ImageFormat::Png);
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#.to_vec();
        let page = b"<html><body>Not found</body></html>".to_vec();
        let base = serve(vec![
            (200, "image/png", png),
            (404, "text/html", page.clone()),
            (200, "text/html", page),
            (200, "image/svg+xml", svg),
        ]);
        let agent = agent();
        let get = |path: &str| load(&Key::Url(format!("{base}{path}")), 1640, Some(&agent), &|| false);
        assert_eq!(size(&get("/a.png").unwrap()), (40, 30));
        assert!(get("/b.png").unwrap_err().ends_with("/b.png: the server answered 404 Not Found"));
        assert!(get("/c.png").unwrap_err().ends_with("/c.png: not an image (got a web page)"));
        assert!(matches!(get("/icon"), Ok(Decoded::Svg(_))), "SVG is recognised by its content type");
        let refused = get("/gone.png").unwrap_err();
        assert!(refused.starts_with(&format!("can't download {base}/gone.png: ")), "{refused}");
    }

    #[test]
    fn loopback_hosts_bypass_the_proxy() {
        for host in ["localhost", "LOCALHOST", "app.localhost", "127.0.0.1", "127.1.2.3", "[::1]", "::1"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["example.com", "10.0.0.1", "[2001:db8::1]", "localhost.example.com"] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    /// Waits for the images being loaded, as the poll timer does in the app.
    fn wait(images: &Images) -> Collected {
        let start = std::time::Instant::now();
        loop {
            let c = images.collect();
            if images.loading.get() == 0 {
                return c;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "images didn't load");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn cache_and_widening() {
        let dir = std::env::temp_dir().join(format!("linter-images-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("wide.png"), encode(&DynamicImage::ImageRgb8(gradient(400, 100)), ImageFormat::Png)).unwrap();
        std::fs::write(dir.join("small.png"), encode(&DynamicImage::ImageRgb8(gradient(40, 10)), ImageFormat::Png)).unwrap();
        let images = Images::new();
        let width = |l: Lookup| match l {
            Lookup::Ready(image) => image.size().width,
            Lookup::Loading => panic!("still loading"),
            Lookup::Failed(why) => panic!("{why}"),
        };

        // Nothing loads until it's asked for.
        assert!(matches!(images.get("wide.png", &dir), Lookup::Loading));
        assert_eq!(images.loading.get(), 0);
        images.request("wide.png", &dir, 100);
        images.request("small.png", &dir, 100);
        images.request("wide.png", &dir, 100);
        assert_eq!(images.loading.get(), 2, "asked for again while loading");
        let c = wait(&images);
        assert!(c.new && !c.sharper);
        assert_eq!(width(images.get("wide.png", &dir)), 100, "scaled down to fit");
        assert_eq!(width(images.get("small.png", &dir)), 40);

        // Wider now: the scaled image is decoded again, and shown meanwhile;
        // the small one is left as it is.
        images.request("wide.png", &dir, 200);
        images.request("small.png", &dir, 200);
        assert_eq!(width(images.get("wide.png", &dir)), 100);
        assert_eq!(images.loading.get(), 1);
        let c = wait(&images);
        assert!(c.sharper && !c.new);
        assert_eq!(width(images.get("wide.png", &dir)), 200);
        images.request("wide.png", &dir, 100);
        assert_eq!(images.loading.get(), 0, "narrower doesn't decode it again");
        assert_eq!(images.bytes.get(), 200 * 50 * 3 + 40 * 10 * 3);

        // Another document: images still loading are dropped.
        std::fs::write(dir.join("wide.png"), encode(&DynamicImage::ImageRgb8(gradient(401, 100)), ImageFormat::Png)).unwrap();
        images.request("wide.png", &dir, 100);
        images.clear();
        std::thread::sleep(Duration::from_millis(200));
        assert!(images.shared.done.lock().unwrap().is_empty(), "the stale image isn't kept");
        assert_eq!(images.bytes.get(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_budget_drops_the_images_asked_for_longest_ago() {
        let dir = std::env::temp_dir().join(format!("linter-images-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.png", "b.png", "c.png"] {
            std::fs::write(dir.join(name), encode(&DynamicImage::ImageRgb8(gradient(100, 100)), ImageFormat::Png)).unwrap();
        }
        let ready = |images: &Images, name: &str| matches!(images.get(name, &dir), Lookup::Ready(_));
        // Room for two of them.
        let images = Images::with_budget(70_000);
        images.request("a.png", &dir, 1640);
        images.request("b.png", &dir, 1640);
        assert!(!wait(&images).evicted);
        images.request("a.png", &dir, 1640); // shown again: b is now the oldest
        images.request("c.png", &dir, 1640);
        assert!(wait(&images).evicted);
        assert!(ready(&images, "a.png") && ready(&images, "c.png"));
        assert!(!ready(&images, "b.png"));
        assert_eq!(images.bytes.get(), 60_000);
        // Asked for again, it loads again.
        images.request("b.png", &dir, 1640);
        assert!(wait(&images).evicted);
        assert!(ready(&images, "b.png") && ready(&images, "c.png") && !ready(&images, "a.png"));

        // An image larger than the budget is still kept, on its own.
        let images = Images::with_budget(1000);
        images.request("a.png", &dir, 1640);
        images.request("b.png", &dir, 1640);
        wait(&images);
        assert_eq!(["a.png", "b.png"].iter().filter(|n| ready(&images, n)).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downloads_of_a_closed_document_dont_hold_up_the_next() {
        // A server that takes connections and never answers.
        let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let slow = format!("http://{}", hang.local_addr().unwrap());
        std::thread::spawn(move || {
            let held: Vec<_> = hang.incoming().collect();
            drop(held);
        });
        let images = Images::new();
        let base = Path::new(".");
        for i in 0..DOWNLOADS * 2 {
            images.request(&format!("{slow}/{i}.png"), base, 1640);
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(images.downloads.get().unwrap().queue.lock().unwrap().busy, DOWNLOADS);

        // Another document: its image comes at once, while the old downloads hang.
        images.clear();
        let png = encode(&DynamicImage::ImageRgb8(gradient(40, 30)), ImageFormat::Png);
        let url = format!("{}/a.png", serve(vec![(200, "image/png", png)]));
        let start = std::time::Instant::now();
        images.request(&url, base, 1640);
        wait(&images);
        assert!(matches!(images.get(&url, base), Lookup::Ready(_)));
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        let pool = images.downloads.get().unwrap().queue.lock().unwrap();
        assert!(pool.jobs.is_empty() && pool.busy == 0);
        assert!(pool.threads <= DOWNLOADS * 2, "{} threads", pool.threads);
    }
}
