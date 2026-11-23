//! The images shown in the document: local files and http(s) URLs.
//!
//! Images are read, downloaded and decoded on worker threads, so a document
//! with many or large images opens at once and typing never waits for one.
//! Until an image is ready its block shows a placeholder. Large images are
//! scaled to what the document column can show while they're decoded, which
//! keeps decoding fast and memory use low. Decoded images are kept until
//! another document is opened.
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
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime};

/// Largest download accepted.
const MAX_DOWNLOAD: u64 = 64 << 20;
/// Largest SVG shown, uncompressed: Slint parses it on the UI thread.
const MAX_SVG: usize = 8 << 20;
/// Largest image decoded, in pixels.
const MAX_PIXELS: u64 = 200_000_000;
/// Longest side of a decoded image: GPUs can't hold larger textures.
const MAX_SIDE: u32 = 16384;
/// Downloads at a time, as browsers allow per host.
const DOWNLOADS: usize = 6;
/// How often finished images are collected while some are loading.
const POLL: Duration = Duration::from_millis(16);
/// AVIF is left out, so that servers that pick a format by this header
/// don't send it.
const ACCEPT: &str = "image/webp,image/png,image/jpeg,image/gif,image/svg+xml,image/*;q=0.8,*/*;q=0.5";

pub enum Lookup {
    Ready(slint::Image),
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

enum Entry {
    Loading,
    /// Decoded for at most `width` pixels wide; `scaled` if it was scaled
    /// down to that.
    Ready { image: slint::Image, width: u32, scaled: bool },
    /// Being decoded again, wider: shown as it is meanwhile.
    Widening(slint::Image),
    Failed(String),
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

/// What `Images::collect` moved into the cache.
#[derive(Default)]
pub struct Collected {
    /// Images, or why they failed, for blocks that waited for them.
    pub new: bool,
    /// Sharper copies of images already shown.
    pub sharper: bool,
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
    /// The image, and whether it was scaled down to `max_width`, the width it
    /// was decoded for.
    fn into_image(self, max_width: u32) -> Result<(slint::Image, bool), String> {
        Ok(match self {
            Decoded::Rgb(b) => {
                let scaled = b.width() >= max_width;
                (slint::Image::from_rgb8(b), scaled)
            }
            Decoded::Rgba(b) => {
                let scaled = b.width() >= max_width;
                (slint::Image::from_rgba8(b), scaled)
            }
            // Rendered at the size it's shown at.
            Decoded::Svg(data) => (slint::Image::load_from_svg_data(&data).map_err(|_| "not a valid SVG image".to_owned())?, false),
        })
    }
}

/// The images of the open document. Lives on the UI thread.
pub struct Images {
    cache: RefCell<HashMap<Key, Entry>>,
    /// Changes when the document does; workers drop jobs of an older one.
    generation: Arc<AtomicU64>,
    done: Arc<Mutex<Vec<Done>>>,
    loading: Cell<usize>,
    files: OnceCell<mpsc::Sender<Job>>,
    downloads: OnceCell<mpsc::Sender<Job>>,
    timer: slint::Timer,
    on_ready: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Images {
    pub fn new() -> Self {
        Images {
            cache: RefCell::default(),
            generation: Arc::default(),
            done: Arc::default(),
            loading: Cell::new(0),
            files: OnceCell::new(),
            downloads: OnceCell::new(),
            timer: slint::Timer::default(),
            on_ready: RefCell::default(),
        }
    }

    /// Called while images are loading, when some may have arrived; it should
    /// call `collect` and show the ones that did.
    pub fn set_on_ready(&self, f: impl Fn() + 'static) {
        *self.on_ready.borrow_mut() = Some(Rc::new(f));
        self.poll();
    }

    /// The image for `url`, relative to `base`, at most `max_width` pixels
    /// wide. Starts loading it the first time.
    pub fn get(&self, url: &str, base: &Path, max_width: u32) -> Lookup {
        let key = match resolve(url, base) {
            Ok(key) => key,
            Err(why) => return Lookup::Failed(why),
        };
        let mut cache = self.cache.borrow_mut();
        match cache.get(&key) {
            Some(Entry::Loading) => return Lookup::Loading,
            Some(Entry::Failed(why)) => return Lookup::Failed(why.clone()),
            Some(Entry::Widening(image)) => return Lookup::Ready(image.clone()),
            Some(Entry::Ready { image, width, scaled }) => {
                let image = image.clone();
                // Shown wider than it was decoded for (the window is on a
                // screen with a larger scale now): decode it again.
                if *scaled && max_width > *width && self.start(key.clone(), max_width) {
                    cache.insert(key, Entry::Widening(image.clone()));
                }
                return Lookup::Ready(image);
            }
            None => {}
        }
        if !self.start(key.clone(), max_width) {
            let why = "can't start loading images".to_owned();
            cache.insert(key, Entry::Failed(why.clone()));
            return Lookup::Failed(why);
        }
        cache.insert(key, Entry::Loading);
        Lookup::Loading
    }

    /// Queues loading `key`. Returns false if there's no thread to load it.
    fn start(&self, key: Key, max_width: u32) -> bool {
        let remote = matches!(key, Key::Url(_));
        let job = Job { generation: self.generation.load(Ordering::Relaxed), key, max_width };
        let pool = if remote { &self.downloads } else { &self.files };
        let sender = pool.get_or_init(|| {
            let threads = if remote {
                DOWNLOADS
            } else {
                // Leave a core for the UI.
                std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1)).clamp(1, 8)
            };
            spawn_pool(threads, remote, self.generation.clone(), self.done.clone())
        });
        if sender.send(job).is_err() {
            return false;
        }
        self.loading.set(self.loading.get() + 1);
        self.poll();
        true
    }

    /// Moves the images that arrived into the cache.
    pub fn collect(&self) -> Collected {
        let done = std::mem::take(&mut *self.done.lock().unwrap());
        let generation = self.generation.load(Ordering::Relaxed);
        let mut collected = Collected::default();
        let mut cache = self.cache.borrow_mut();
        for d in done.into_iter().filter(|d| d.generation == generation) {
            let widening = match cache.remove(&d.key) {
                Some(Entry::Widening(image)) => Some(image),
                _ => None,
            };
            let entry = match (d.result.and_then(|decoded| decoded.into_image(d.max_width)), widening) {
                (Ok((image, scaled)), widening) => {
                    collected.sharper |= widening.is_some();
                    collected.new |= widening.is_none();
                    Entry::Ready { image, width: d.max_width, scaled }
                }
                // Keep showing the image it already has, and don't try again.
                (Err(_), Some(image)) => Entry::Ready { image, width: d.max_width, scaled: false },
                (Err(why), None) => {
                    collected.new = true;
                    Entry::Failed(why)
                }
            };
            cache.insert(d.key, entry);
            self.loading.set(self.loading.get().saturating_sub(1));
        }
        if self.loading.get() == 0 {
            self.timer.stop();
        }
        collected
    }

    /// Forgets all images, for another document. Images still loading are
    /// dropped when they arrive.
    pub fn clear(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        // After the generation changes, so that workers add no more of them.
        self.done.lock().unwrap().clear();
        self.cache.borrow_mut().clear();
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

fn spawn_pool(threads: usize, remote: bool, generation: Arc<AtomicU64>, done: Arc<Mutex<Vec<Done>>>) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    let agent = remote.then(agent);
    for i in 0..threads {
        let (rx, generation, done, agent) = (rx.clone(), generation.clone(), done.clone(), agent.clone());
        let name = format!("{}-{i}", if remote { "download" } else { "image" });
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            loop {
                let Ok(job) = rx.lock().unwrap().recv() else { return };
                if job.generation != generation.load(Ordering::Relaxed) {
                    continue;
                }
                // A decoder bug on a bad file shouldn't leave the image loading forever.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(&job.key, job.max_width, agent.as_ref())))
                    .unwrap_or_else(|_| Err(format!("{}: the decoder failed", job.key.name())));
                // The document may have changed meanwhile: then `clear` has
                // emptied `done`, and nothing would take the image out again.
                let mut done = done.lock().unwrap();
                if job.generation == generation.load(Ordering::Relaxed) {
                    done.push(Done { generation: job.generation, key: job.key, max_width: job.max_width, result });
                }
            }
        });
        if spawned.is_err() && i == 0 {
            // No thread to run the jobs: dropping `rx` makes `send` fail.
            break;
        }
    }
    tx
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

/// Runs on a worker thread.
fn load(key: &Key, max_width: u32, agent: Option<&ureq::Agent>) -> Result<Decoded, String> {
    let (bytes, ext) = match key {
        Key::File { path, .. } => {
            let bytes = std::fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
            (bytes, path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()))
        }
        Key::Url(url) => {
            let agent = agent.ok_or("no downloader")?;
            let (bytes, content_type) = fetch(agent, url).map_err(|why| format!("can't download {url}: {why}"))?;
            let ext = match content_type.as_deref() {
                Some(t) if t.starts_with("image/svg") => Some("svg".to_owned()),
                _ => url_extension(url),
            };
            (bytes, ext)
        }
    };
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

/// The body and content type of `url`.
fn fetch(agent: &ureq::Agent, url: &str) -> Result<(Vec<u8>, Option<String>), String> {
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
    let bytes = response.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().map_err(|e| describe(&e))?;
    Ok((bytes, content_type))
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
        let get = |path: &str| load(&Key::Url(format!("{base}{path}")), 1640, Some(&agent));
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

        assert!(matches!(images.get("wide.png", &dir, 100), Lookup::Loading));
        assert!(matches!(images.get("small.png", &dir, 100), Lookup::Loading));
        let c = wait(&images);
        assert!(c.new && !c.sharper);
        assert_eq!(width(images.get("wide.png", &dir, 100)), 100, "scaled down to fit");
        assert_eq!(width(images.get("small.png", &dir, 100)), 40);

        // Wider now: the scaled image is decoded again, and shown meanwhile;
        // the small one is left as it is.
        assert_eq!(width(images.get("wide.png", &dir, 200)), 100);
        assert_eq!(width(images.get("small.png", &dir, 200)), 40);
        assert_eq!(images.loading.get(), 1);
        let c = wait(&images);
        assert!(c.sharper && !c.new);
        assert_eq!(width(images.get("wide.png", &dir, 200)), 200);
        assert_eq!(width(images.get("wide.png", &dir, 100)), 200, "narrower doesn't decode it again");
        assert_eq!(images.loading.get(), 0);

        // Another document: images still loading are dropped.
        std::fs::write(dir.join("wide.png"), encode(&DynamicImage::ImageRgb8(gradient(401, 100)), ImageFormat::Png)).unwrap();
        assert!(matches!(images.get("wide.png", &dir, 100), Lookup::Loading));
        images.clear();
        std::thread::sleep(Duration::from_millis(200));
        assert!(images.done.lock().unwrap().is_empty(), "the stale image isn't kept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
