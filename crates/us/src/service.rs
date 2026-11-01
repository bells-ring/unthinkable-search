//! Index service: load (or build) the index and keep it fresh in a background
//! thread. One implementation shared by the GUI, `usc serve` and the DLL.
//!
//! Freshness: elevated process → USN journals read in-process; otherwise the
//! privilege-separated broker (`us.exe --broker`, one UAC prompt) streams
//! metadata changes. Declined UAC / `Live::Off` → the loaded index as is
//! (built by directory walk if there is none).
//!
//! Readers call [`Service::index`] and get an immutable `Arc<Index>` snapshot:
//! searches never block updates and never see a half-applied change.

use crate::broker;
use crate::engine::{Live, Updater};
use crate::is_elevated;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use us_core::{store, Index};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveMode {
    /// Load the saved index only (build by directory walk if missing, MFT if
    /// elevated). Never shows a UAC prompt.
    Off,
    /// Live updates: in-process when elevated, else via the broker (UAC prompt).
    Auto,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Folder holding `index.usdb` (created on save).
    pub data_dir: PathBuf,
    pub live: LiveMode,
    /// Executable that accepts `--broker` (us.exe or usc.exe), launched elevated.
    pub broker_exe: PathBuf,
}

impl Options {
    /// Defaults for a program living next to `us.exe`: `<exe dir>\data`, live.
    pub fn next_to_exe() -> Options {
        let exe = std::env::current_exe().unwrap_or_default();
        // us.exe and usc.exe both accept `--broker`: the broker is this very exe.
        Options { data_dir: crate::data_dir_from_exe(), live: LiveMode::Auto, broker_exe: exe }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Loading,
    Indexing,
    Ready,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Loading => "loading",
            State::Indexing => "indexing",
            State::Ready => "ready",
        }
    }
}

/// Notifications from the service thread (called on that thread).
pub enum Event {
    /// A new index snapshot was published (`Service::index`).
    Index,
    /// State / status message changed (`Service::status`).
    Status,
}

struct Inner {
    ix: RwLock<Option<Arc<Index>>>,
    generation: AtomicU64,
    status: Mutex<(State, bool, String)>,
    ready_cv: Condvar,
    quit: AtomicBool,
}

pub struct Service {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl Service {
    /// Start the service thread. `notify` runs on the service thread.
    pub fn start(opts: Options, notify: impl Fn(Event) + Send + 'static) -> Service {
        let inner = Arc::new(Inner {
            ix: RwLock::new(None),
            generation: AtomicU64::new(0),
            status: Mutex::new((State::Loading, false, "Загрузка индекса…".into())),
            ready_cv: Condvar::new(),
            quit: AtomicBool::new(false),
        });
        let i2 = inner.clone();
        let thread = std::thread::Builder::new()
            .name("us-index".into())
            .spawn(move || worker(&i2, &opts, &notify))
            .expect("spawn index thread");
        Service { inner, thread: Some(thread) }
    }

    /// Latest published snapshot (None until the first load/build finishes).
    pub fn index(&self) -> Option<Arc<Index>> {
        self.inner.ix.read().unwrap().clone()
    }

    /// Incremented on every published snapshot (0 = none yet).
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    /// (state, live updates active, human-readable status in Russian).
    pub fn status(&self) -> (State, bool, String) {
        self.inner.status.lock().unwrap().clone()
    }

    /// Wait until an index is available (or `timeout`).
    pub fn wait_index(&self, timeout: Duration) -> Option<Arc<Index>> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.status.lock().unwrap();
        loop {
            if let Some(ix) = self.index() {
                return Some(ix);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            g = self.inner.ready_cv.wait_timeout(g, left.min(Duration::from_millis(100))).unwrap().0;
        }
    }

    /// Stop the thread and save the index (keeps USN cursors for a fast restart).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.inner.quit.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(inner: &Inner, opts: &Options, notify: &dyn Fn(Event)) {
    let path = opts.data_dir.join("index.usdb");
    let t = Instant::now();
    let loaded = store::load(&path).ok();
    let loaded_ms = t.elapsed().as_secs_f64() * 1e3;
    let have_index = loaded.is_some();
    let publish = |ix: &Arc<Index>| {
        if !ix.is_empty() {
            // Folder sizes and the directory tree (path queries) are lazy; build
            // them here, not on a reader's thread.
            ix.total_size(ix.volumes.first().map_or(0, |v| v.root_entry));
            *inner.ix.write().unwrap() = Some(ix.clone());
            // Temporary load/rebuild buffers in the allocator cache are useless for searches.
            us_core::alloc::trim();
            inner.generation.fetch_add(1, Ordering::SeqCst);
            inner.ready_cv.notify_all();
            notify(Event::Index);
        }
    };
    let set = |state: State, live: bool, s: String| {
        *inner.status.lock().unwrap() = (state, live, s);
        inner.ready_cv.notify_all();
        notify(Event::Status);
    };
    let save = |ix: &Index| {
        let _ = std::fs::create_dir_all(&opts.data_dir);
        let _ = store::save(ix, &path);
    };
    let quit = || inner.quit.load(Ordering::Relaxed);
    let ready_msg = |n: usize, how: &str| format!("Индекс: {n} объектов (загрузка {loaded_ms:.0} мс) · {how}");

    // --- Elevated: read the journals in-process.
    if is_elevated() {
        let ix = match loaded {
            Some(ix) => ix,
            None => {
                set(State::Indexing, false, "Индексация (MFT)…".into());
                crate::cli::build_index_all(false)
            }
        };
        if opts.live == LiveMode::Off {
            let ix = Arc::new(ix);
            publish(&ix);
            set(State::Ready, false, ready_msg(ix.len(), "живые обновления выключены"));
            idle(&quit);
            if !have_index {
                save(&ix);
            }
            return;
        }
        let mut live = Live::new(ix);
        publish(live.ix());
        set(State::Ready, true, ready_msg(live.ix().len(), &format!("живые обновления: {} том(а)", live.watching())));
        let mut dirty = !have_index;
        let mut last_save = Instant::now();
        while !quit() {
            live.poll();
            if live.maybe_apply(Duration::from_millis(100)).is_some() {
                publish(live.ix());
                dirty = true;
            }
            live.log.clear();
            if dirty && last_save.elapsed() > Duration::from_secs(120) {
                save(live.ix());
                (dirty, last_save) = (false, Instant::now());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        save(live.ix()); // keeps the USN cursor → next start only replays the delta
        return;
    }

    // --- Not elevated.
    let mut up = Updater::new(loaded.unwrap_or_default());
    publish(&up.ix);
    let mut dirty = !have_index;
    let started = if opts.live == LiveMode::Off {
        Err(std::io::Error::other("режим --live off"))
    } else {
        set(
            if have_index { State::Ready } else { State::Indexing },
            false,
            if have_index { ready_msg(up.ix.len(), "запрос прав для живых обновлений (UAC)…") } else { "Запрос прав для индексации MFT (UAC)…".into() },
        );
        broker::Client::start_with(&up.ix, &opts.broker_exe)
    };
    match started {
        Ok(client) => {
            let (mut ready, mut log) = (false, Vec::new());
            let mut last_save = Instant::now();
            while !quit() {
                let (alive, scanned) = broker::pump(&client.rx, &mut up, Duration::from_millis(50), &mut log, &mut ready);
                if scanned {
                    publish(&up.ix);
                    dirty = true;
                }
                if ready {
                    ready = false;
                    set(State::Ready, true, ready_msg(up.ix.len(), "живые обновления через брокер"));
                }
                if up.maybe_apply(Duration::from_millis(100)).is_some() {
                    publish(&up.ix);
                    dirty = true;
                }
                log.clear();
                if dirty && last_save.elapsed() > Duration::from_secs(120) {
                    save(&up.ix);
                    (dirty, last_save) = (false, Instant::now());
                }
                if !alive {
                    set(State::Ready, false, ready_msg(up.ix.len(), "брокер завершился: живые обновления выключены"));
                    break;
                }
            }
        }
        Err(e) => {
            if !have_index {
                set(State::Indexing, false, "Индексация обходом каталогов (без прав)…".into());
                up = Updater::new(crate::cli::build_index_all(true));
                publish(&up.ix);
            }
            set(State::Ready, false, ready_msg(up.ix.len(), &format!("живые обновления выключены ({e})")));
        }
    }
    idle(&quit);
    if dirty && !up.ix.is_empty() {
        save(&up.ix);
    }
}

fn idle(quit: &dyn Fn() -> bool) {
    while !quit() {
        std::thread::sleep(Duration::from_millis(50));
    }
}
