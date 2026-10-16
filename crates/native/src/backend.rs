//! [`NativeBackend`]: the desktop implementation of the UI's [`Backend`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use artcraft_launcher_core::feed::{Feed, FeedAnswer};
use artcraft_launcher_core::platform::{Arch, Os, Platform, pick_asset};
use artcraft_launcher_core::protocol::{
    Backend, BackendOptions, CANCELLED, Capabilities, Command, Event, INSTALL_CANCELLED, JobPhase, LatestRelease, Origin, PAUSED, PausedDownload,
    Source,
};
use artcraft_launcher_core::release::{Asset, Release};
use artcraft_launcher_core::{CraftApp, Installed, PackageKind, SUITE, version};

use crate::http::{self, Http, Stop};
use crate::install::{self, Job};
use crate::manifest::Manifest;
use crate::paths::Paths;
use crate::source::{GitHubSource, ReleaseSource};

/// Automatic checks closer together than this reuse the last answers: it stops a burst of
/// requests (refresh, timers) from repeating work.
const FRESH_FOR: i64 = 4 * 60;
/// Requests in flight at once during a check.
const WORKERS: usize = 4;

type Waker = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    events: Mutex<Vec<Event>>,
    waker: Waker,
    latest: Mutex<HashMap<String, Release>>,
    installed: Mutex<HashMap<String, Installed>>,
    jobs: Mutex<HashMap<String, Arc<Stop>>>,
    manifest: Mutex<Manifest>,
    options: Mutex<BackendOptions>,
    checking: AtomicBool,
    /// A detection pass is running; another request waits for the next one.
    detecting: AtomicBool,
    last_check: Mutex<Option<i64>>,
}

impl Shared {
    fn emit(&self, event: Event) {
        self.events.lock().unwrap_or_else(PoisonError::into_inner).push(event);
        (self.waker)();
    }
}

pub struct NativeBackend {
    shared: Arc<Shared>,
    paths: Paths,
    http: Option<Http>,
    source: Option<Arc<dyn ReleaseSource>>,
    platform: Option<Platform>,
}

impl NativeBackend {
    /// `waker` is called (from any thread) whenever events are waiting; it should repaint the UI.
    pub fn new(waker: impl Fn() + Send + Sync + 'static) -> Self {
        let waker: Waker = Arc::new(waker);
        let paths = Paths::new().unwrap_or_else(|e| {
            log::error!("{e}; keeping files in the temporary folder");
            Paths::under(&std::env::temp_dir().join("artcraft-launcher"))
        });
        let http = match Http::new() {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("HTTPS unavailable: {e}");
                None
            }
        };
        let source: Option<Arc<dyn ReleaseSource>> =
            http.clone().map(|h| Arc::new(GitHubSource::new(h, paths.release_cache())) as Arc<dyn ReleaseSource>);
        let shared = Arc::new(Shared {
            events: Mutex::new(Vec::new()),
            waker,
            latest: Mutex::new(HashMap::new()),
            installed: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            manifest: Mutex::new(Manifest::load(&paths.manifest())),
            options: Mutex::new(BackendOptions::default()),
            checking: AtomicBool::new(false),
            detecting: AtomicBool::new(false),
            last_check: Mutex::new(None),
        });
        let backend = Self { shared, paths, http, source, platform: native_platform() };
        backend.replay_cache();
        backend.detect_all();
        backend
    }

    /// Shows GitHub's saved answers from the last run straight away, before any network request.
    fn replay_cache(&self) {
        let Some(source) = &self.source else { return };
        let pre = self.options().prereleases;
        for app in &SUITE {
            let Some(latest) = source.cached(app.repo, pre) else { continue };
            if let LatestRelease::Found(r) = &latest {
                self.shared.latest.lock().unwrap_or_else(PoisonError::into_inner).insert(app.id.to_string(), r.clone());
            }
            self.shared.emit(Event::Latest { app: app.id.to_string(), latest, from: Origin::GitHub });
        }
    }

    fn options(&self) -> BackendOptions {
        self.shared.options.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn detect_all(&self) {
        if self.shared.detecting.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = self.shared.clone();
        let paths = self.paths.clone();
        let platform = self.platform;
        let options = self.options();
        std::thread::spawn(move || {
            // Leftovers from interrupted installs or older versions of the launcher. A paused
            // download stays a month; a finished one kept after a failed install, a week.
            http::clean_downloads(&paths.downloads(), Duration::from_secs(30 * 86_400), Duration::from_secs(7 * 86_400));
            let snapshot = crate::detect::Snapshot::take();
            let manifest = shared.manifest.lock().unwrap_or_else(PoisonError::into_inner).clone();
            for app in &SUITE {
                let found = crate::detect::detect(app, &manifest, &snapshot);
                {
                    let mut map = shared.installed.lock().unwrap_or_else(PoisonError::into_inner);
                    match &found {
                        Some(i) => map.insert(app.id.to_string(), i.clone()),
                        None => map.remove(app.id),
                    };
                }
                shared.emit(Event::Detected { app: app.id.to_string(), installed: found });
            }
            // Downloads paused (or cut off by quitting) in an earlier run.
            for app in &SUITE {
                report_partial(app, &shared, &paths, platform, &options);
            }
            shared.detecting.store(false, Ordering::SeqCst);
        });
    }

    /// Looks up the latest releases on GitHub, each app at its own repo in the catalog (or on a
    /// developer's local mirror, when one is set). ArtCraft's release feed is never asked.
    fn check(&self, force: bool) {
        let (Some(source), Some(http)) = (self.source.clone(), self.http.clone()) else {
            for app in &SUITE {
                self.shared.emit(Event::Latest {
                    app: app.id.to_string(),
                    latest: LatestRelease::Failed("secure connections aren't available".into()),
                    from: Origin::GitHub,
                });
            }
            return;
        };
        let now = crate::now();
        let recent = self.shared.last_check.lock().unwrap_or_else(PoisonError::into_inner).is_some_and(|t| now - t < FRESH_FOR);
        if (recent && !force) || self.shared.checking.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = self.shared.clone();
        let paths = self.paths.clone();
        let platform = self.platform;
        let pre = self.options().prereleases;
        let mirror = local_mirror();
        std::thread::spawn(move || {
            shared.emit(Event::CheckStarted);
            // A local mirror answers for everything: GitHub is never asked.
            let feed = mirror.as_ref().and_then(|base| local_feed(&http, base));
            if let Some(f) = &feed {
                shared.emit(Event::Notices { announcements: f.announcements.clone(), alerts: f.alerts.clone() });
            }
            let now = crate::now();
            // The launcher's own repo is never asked: self-update is off in this build.
            let queue = Mutex::new(SUITE.iter().collect::<Vec<&CraftApp>>());
            let sources = Mutex::new(Vec::new());
            std::thread::scope(|scope| {
                for _ in 0..WORKERS {
                    scope.spawn(|| {
                        loop {
                            let Some(app) = queue.lock().unwrap_or_else(PoisonError::into_inner).pop() else { break };
                            let (latest, from, used) = match &mirror {
                                Some(_) => mirror_latest(app, feed.as_ref(), now),
                                None => github_latest(app, source.as_ref(), pre),
                            };
                            if let Some(used) = used {
                                sources.lock().unwrap_or_else(PoisonError::into_inner).push(used);
                            }
                            let Some(latest) = latest else { continue };
                            if let LatestRelease::Found(r) = &latest {
                                shared.latest.lock().unwrap_or_else(PoisonError::into_inner).insert(app.id.to_string(), r.clone());
                            }
                            shared.emit(Event::Latest { app: app.id.to_string(), latest, from });
                        }
                    });
                }
            });
            source.save();
            // A newer release changes which file an install would use.
            let options = shared.options.lock().unwrap_or_else(PoisonError::into_inner).clone();
            for app in &SUITE {
                report_partial(app, &shared, &paths, platform, &options);
            }
            let sources = sources.into_inner().unwrap_or_else(PoisonError::into_inner);
            let source_used = if sources.contains(&Source::Cache) {
                Source::Cache
            } else if sources.contains(&Source::GitHubPages) {
                Source::GitHubPages
            } else if sources.contains(&Source::GitHubApi) {
                Source::GitHubApi
            } else {
                Source::Feed
            };
            let at = crate::now();
            *shared.last_check.lock().unwrap_or_else(PoisonError::into_inner) = Some(at);
            shared.checking.store(false, Ordering::SeqCst);
            let rate_limited_until = limit_notice(source_used, source.rate_limited_until());
            shared.emit(Event::CheckFinished { at, source: source_used, rate_limited_until });
        });
    }

    fn start_job(&self, app_id: &str, uninstall: bool) {
        let Some(app) = CraftApp::by_id(app_id) else { return };
        log::info!("{}: starting {} job", app.name, if uninstall { "uninstall" } else { "install/update" });
        let stop = Arc::new(Stop::default());
        {
            let mut jobs = self.shared.jobs.lock().unwrap_or_else(PoisonError::into_inner);
            if jobs.contains_key(app.id) {
                return;
            }
            jobs.insert(app.id.to_string(), stop.clone());
        }
        let shared = self.shared.clone();
        let paths = self.paths.clone();
        let http = self.http.clone();
        let platform = self.platform;
        let options = self.options();
        std::thread::spawn(move || {
            let existing = shared.installed.lock().unwrap_or_else(PoisonError::into_inner).get(app.id).cloned();
            let record = shared.manifest.lock().unwrap_or_else(PoisonError::into_inner).apps.get(app.id).cloned();
            let result = if uninstall {
                shared.emit(Event::Progress { app: app.id.to_string(), phase: JobPhase::Removing });
                let mut phase = |phase| shared.emit(Event::Progress { app: app.id.to_string(), phase });
                match &existing {
                    Some(inst) => install::uninstall(app, inst, record.as_ref(), &stop, &mut phase).map(|()| {
                        let mut m = shared.manifest.lock().unwrap_or_else(PoisonError::into_inner);
                        if m.apps.remove(app.id).is_some() {
                            let _ = m.save(&paths.manifest());
                        }
                        format!("{} was removed", app.name)
                    }),
                    None => Err(format!("{} isn't installed", app.name)),
                }
            } else {
                let r = run_install(app, &shared, &paths, http.as_ref(), platform, &options, existing.as_ref(), record.as_ref(), &stop);
                // Stopped by the connection rather than by the user: say so, and how to go on.
                match r {
                    Err(e) if e == PAUSED && stop.check().is_ok() => {
                        Err("The download paused because the connection keeps dropping. Press Resume to continue where it stopped.".into())
                    }
                    other => other,
                }
            };
            report_partial(app, &shared, &paths, platform, &options);
            // Look again: the result is whatever is on disk now.
            let manifest = shared.manifest.lock().unwrap_or_else(PoisonError::into_inner).clone();
            let found = crate::detect::detect(app, &manifest, &crate::detect::Snapshot::take());
            // One installer call can cover several apps, and fail after some of them went in.
            let latest = shared.latest.lock().unwrap_or_else(PoisonError::into_inner).get(app.id).cloned();
            let result = match (result, latest) {
                (Err(e), Some(release))
                    if !uninstall && release.version.as_ref().is_some_and(|v| landed_despite(&e, existing.as_ref(), found.as_ref(), v)) =>
                {
                    log::warn!("{}: {e}; the new version is installed all the same", app.id);
                    Ok(installed_text(app, existing.is_some(), &release))
                }
                (result, _) => result,
            };
            {
                let mut map = shared.installed.lock().unwrap_or_else(PoisonError::into_inner);
                match &found {
                    Some(i) => map.insert(app.id.to_string(), i.clone()),
                    None => map.remove(app.id),
                };
            }
            shared.emit(Event::Detected { app: app.id.to_string(), installed: found });
            shared.jobs.lock().unwrap_or_else(PoisonError::into_inner).remove(app.id);
            shared.emit(Event::JobFinished { app: app.id.to_string(), result });
        });
    }

    fn installed(&self, app: &str) -> Option<Installed> {
        self.shared.installed.lock().unwrap_or_else(PoisonError::into_inner).get(app).cloned()
    }
}

#[allow(clippy::too_many_arguments)]
fn run_install(
    app: &CraftApp,
    shared: &Shared,
    paths: &Paths,
    http: Option<&Http>,
    platform: Option<Platform>,
    options: &BackendOptions,
    existing: Option<&Installed>,
    record: Option<&crate::manifest::Record>,
    stop: &Stop,
) -> Result<String, String> {
    if let Some(msg) = existing.and_then(|e| install::managed_elsewhere(app, e, false)) {
        return Err(msg);
    }
    let http = http.ok_or("secure connections aren't available on this system")?;
    let platform = platform.ok_or("this system isn't supported")?;
    let release = shared
        .latest
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(app.id)
        .cloned()
        .ok_or("the latest release hasn't been found yet. Check for updates first.")?;
    let (asset, kind) = planned_asset(app, &release, platform, options, existing, record)
        .ok_or_else(|| format!("{} {} has no download for {}", app.name, release.version_label(), platform.label()))?;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut phase = |p: JobPhase| {
        // Downloads report thousands of times; the UI needs ~20 a second.
        if matches!(p, JobPhase::Downloading { done, total } if done != total && done != 0) && last_emit.elapsed() < Duration::from_millis(50) {
            return;
        }
        last_emit = Instant::now();
        shared.emit(Event::Progress { app: app.id.to_string(), phase: p });
    };
    let mut job = Job { http, paths, stop, phase: &mut phase };
    let (new_record, verified) = install::install(app, &release, &asset, kind, existing, record, &mut job)?;
    {
        let mut m = shared.manifest.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = match new_record {
            Some(r) => {
                m.apps.insert(app.id.to_string(), r);
                true
            }
            // Installed the platform's own way: any old launcher record is stale.
            None => m.apps.remove(app.id).is_some(),
        };
        if changed && let Err(e) = m.save(&paths.manifest()) {
            log::warn!("couldn't save the install record: {e}");
        }
    }
    let note = match verified {
        http::Verified::Checksum => "",
        http::Verified::SizeOnly => " (this release publishes no checksum, so only its size was checked)",
    };
    Ok(format!("{}{note}", installed_text(app, existing.is_some(), &release)))
}

/// What a finished install tells the user.
fn installed_text(app: &CraftApp, update: bool, release: &Release) -> String {
    let verb = if update { "updated to" } else { "installed:" };
    format!("{} {verb} {}", app.name, release.version_label())
}

/// Whether an install whose job failed with `error` went in all the same. Only a failed call
/// shared by several apps (Linux packages installed together) can have installed some of them;
/// any other error, a cancellation or a pause stands, whatever is found afterwards.
fn landed_despite(error: &str, before: Option<&Installed>, after: Option<&Installed>, latest: &version::Version) -> bool {
    error != PAUSED && error != CANCELLED && error != INSTALL_CANCELLED && install::failed_in_shared_call(error) && landed(before, after, latest)
}

/// Whether an install took, whatever its job reported: `latest` is there now and wasn't before.
fn landed(before: Option<&Installed>, after: Option<&Installed>, latest: &version::Version) -> bool {
    let has_latest = |copy: Option<&Installed>| copy.and_then(|i| i.version.as_ref()).is_some_and(|v| !version::is_update(latest, v));
    has_latest(after) && !has_latest(before)
}

/// The file an install of `app` would download now: same format as the installed copy, else
/// the user's preference, else the best this system supports.
fn planned_asset(
    app: &CraftApp,
    release: &Release,
    platform: Platform,
    options: &BackendOptions,
    existing: Option<&Installed>,
    record: Option<&crate::manifest::Record>,
) -> Option<(Asset, PackageKind)> {
    let stems: Vec<&str> = app.stems().collect();
    let kinds = install::choose_kinds(existing, record, &options.kinds);
    pick_asset(release, &stems, platform, &kinds).map(|(a, k)| (a.clone(), k))
}

/// Tells the UI whether `app` has a paused download waiting, or a finished one to install.
fn report_partial(app: &CraftApp, shared: &Shared, paths: &Paths, platform: Option<Platform>, options: &BackendOptions) {
    let planned = planned_download(app, shared, paths, platform, options);
    let download = planned.as_ref().and_then(|(dest, size)| Some(PausedDownload { done: http::partial_bytes(dest, *size)?, total: *size }));
    shared.emit(Event::Paused { app: app.id.to_string(), download });
    let ready = planned.is_some_and(|(dest, size)| http::finished(&dest, size));
    shared.emit(Event::ReadyToInstall { app: app.id.to_string(), ready });
}

/// Where the planned download of `app` is kept, and its size.
fn planned_download(
    app: &CraftApp,
    shared: &Shared,
    paths: &Paths,
    platform: Option<Platform>,
    options: &BackendOptions,
) -> Option<(std::path::PathBuf, u64)> {
    let release = shared.latest.lock().unwrap_or_else(PoisonError::into_inner).get(app.id).cloned()?;
    let existing = shared.installed.lock().unwrap_or_else(PoisonError::into_inner).get(app.id).cloned();
    let record = shared.manifest.lock().unwrap_or_else(PoisonError::into_inner).apps.get(app.id).cloned();
    let (asset, _) = planned_asset(app, &release, platform?, options, existing.as_ref(), record.as_ref())?;
    artcraft_launcher_core::release::valid_file_name(&asset.name).then(|| (install::download_path(paths, &asset), asset.size))
}

/// The latest release of `app` from GitHub, at the repo the catalog names for it, and the
/// GitHub route used (for the rate-limit notice).
fn github_latest(app: &CraftApp, source: &dyn ReleaseSource, pre: bool) -> (Option<LatestRelease>, Origin, Option<Source>) {
    let answer = source.latest(app.repo, pre);
    (Some(answer.latest), Origin::GitHub, Some(answer.source))
}

/// Until when the footer says GitHub's limit holds details back: only while it does, that is
/// when this check had to settle for the release pages or saved answers. When the API itself
/// (or a local mirror) answered everything, nothing is missing, however recently GitHub said no.
fn limit_notice(source_used: Source, limited_until: Option<i64>) -> Option<i64> {
    limited_until.filter(|_| matches!(source_used, Source::GitHubPages | Source::Cache))
}

/// `http://<loopback host>:<port>` and an optional path: the host must be exactly 127.0.0.1,
/// localhost or [::1] (no `user@` in front of another host), with a numeric port.
fn is_loopback(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let Some((host, port)) = authority.rsplit_once(':') else { return false };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]") && !port.is_empty() && port.parse::<u16>().is_ok()
}

/// `ARTCRAFT_LAUNCHER_DOWNLOAD_BASE`: a mirror on this machine (`cargo xtask local serve`) that
/// serves `release_info.json` and the builds it lists. While it's set, releases come only from
/// there (never GitHub or ArtCraft's API), and its download links are trusted. Loopback only, so
/// nothing but the developer's own builds can be installed this way, and only in builds with the
/// `local-mirror` feature: release builds ignore the variable.
fn local_mirror() -> Option<String> {
    if !cfg!(feature = "local-mirror") {
        return None;
    }
    let base = std::env::var("ARTCRAFT_LAUNCHER_DOWNLOAD_BASE").ok()?;
    let base = base.trim();
    if !is_loopback(base) || base.contains(['?', '#', '\\']) {
        log::warn!(
            "ignoring ARTCRAFT_LAUNCHER_DOWNLOAD_BASE={base}: only http://127.0.0.1:<port>/, http://localhost:<port>/ or http://[::1]:<port>/"
        );
        return None;
    }
    Some(if base.ends_with('/') { base.to_string() } else { format!("{base}/") })
}

/// The local mirror's feed: `release_info.json` beside the builds. Asked every check (it's on
/// this machine) and never saved.
fn local_feed(http: &Http, base: &str) -> Option<Feed> {
    let url = format!("{base}release_info.json");
    match http.get_feed(&url) {
        Ok(body) => {
            let feed = Feed::parse(&body).map(|f| f.with_mirror(base));
            if feed.is_none() {
                log::warn!("the local mirror's feed at {url} is unreadable");
            }
            feed
        }
        Err(e) => {
            log::warn!("local mirror unavailable: {e} (is `cargo xtask local serve` running?)");
            None
        }
    }
}

/// `app`'s latest release according to the local mirror. An app it doesn't list has no release.
fn mirror_latest(app: &CraftApp, feed: Option<&Feed>, now: i64) -> (Option<LatestRelease>, Origin, Option<Source>) {
    let latest = match feed.map(|f| f.answer(app, now)) {
        Some(FeedAnswer::Fresh(Some(r))) => LatestRelease::Found(r),
        Some(FeedAnswer::Fresh(None) | FeedAnswer::AskGitHub) => LatestRelease::None,
        None => LatestRelease::Failed("the local mirror didn't answer".into()),
    };
    (Some(latest), Origin::LocalMirror, Some(Source::Feed))
}

/// The machine's real architecture: an x64 launcher on Windows on ARM should still pick ARM64
/// builds.
fn native_platform() -> Option<Platform> {
    let mut p = Platform::current()?;
    if p.os == Os::Windows {
        let arch = std::env::var("PROCESSOR_ARCHITEW6432").or_else(|_| std::env::var("PROCESSOR_ARCHITECTURE")).unwrap_or_default();
        if arch.eq_ignore_ascii_case("ARM64") {
            p.arch = Arch::Arm64;
        } else if arch.eq_ignore_ascii_case("AMD64") {
            p.arch = Arch::X64;
        }
    }
    Some(p)
}

impl Backend for NativeBackend {
    fn send(&mut self, command: Command) {
        match command {
            Command::CheckForUpdates { force } => self.check(force),
            Command::Detect => self.detect_all(),
            Command::Install { app } => self.start_job(&app, false),
            Command::Uninstall { app } => self.start_job(&app, true),
            Command::Pause { app } => {
                if let Some(stop) = self.shared.jobs.lock().unwrap_or_else(PoisonError::into_inner).get(&app) {
                    stop.pause();
                }
            }
            Command::Cancel { app } => {
                let running = self.shared.jobs.lock().unwrap_or_else(PoisonError::into_inner).get(&app).cloned();
                match running {
                    Some(stop) => stop.cancel(),
                    // A paused download, or one kept after a failed install: throw the file away.
                    None => {
                        if let Some(def) = CraftApp::by_id(&app)
                            && let Some((dest, size)) = planned_download(def, &self.shared, &self.paths, self.platform, &self.options())
                        {
                            http::discard(&dest, size);
                        }
                        self.shared.emit(Event::Paused { app: app.clone(), download: None });
                        self.shared.emit(Event::ReadyToInstall { app, ready: false });
                    }
                }
            }
            Command::Launch { app } => {
                let Some(def) = CraftApp::by_id(&app) else { return };
                let result = match self.installed(&app) {
                    Some(inst) => install::launch(def, &inst),
                    None => Err(format!("{} isn't installed", def.name)),
                };
                if let Err(e) = result {
                    self.shared.emit(Event::JobFinished { app, result: Err(e) });
                }
            }
            Command::Reveal { app } => {
                if let Some(inst) = self.installed(&app) {
                    let path = std::path::PathBuf::from(&inst.location);
                    let result = if path.exists() { crate::process::reveal(&path) } else { Err("it isn't a file on this computer".into()) };
                    if let Err(e) = result {
                        self.shared.emit(Event::JobFinished { app, result: Err(format!("Couldn't show it: {e}")) });
                    }
                }
            }
            // Self-update is off in this build: new launcher versions are handed out by hand.
            Command::UpdateLauncher => self.shared.emit(Event::JobFinished {
                app: artcraft_launcher_core::catalog::LAUNCHER_ID.to_string(),
                result: Err("this build of ArtCraft Launcher doesn't update itself".into()),
            }),
            Command::Configure(options) => {
                let changed_pre = {
                    let mut o = self.shared.options.lock().unwrap_or_else(PoisonError::into_inner);
                    let changed = o.prereleases != options.prereleases;
                    *o = options;
                    changed
                };
                // Another package format means another file to download.
                let options = self.options();
                for app in &SUITE {
                    report_partial(app, &self.shared, &self.paths, self.platform, &options);
                }
                if changed_pre {
                    self.check(true);
                }
            }
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut *self.shared.events.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn capabilities(&self) -> Capabilities {
        let reveal_label = match self.platform.map(|p| p.os) {
            Some(Os::MacOs) => "Show in Finder",
            Some(Os::Windows) => "Show in Explorer",
            _ => "Show in Files",
        };
        Capabilities { manage_apps: true, reveal_label, self_update: false }
    }

    fn platform(&self) -> Option<Platform> {
        self.platform
    }

    fn now(&self) -> i64 {
        crate::now()
    }
}

#[cfg(test)]
mod tests {
    use artcraft_launcher_core::InstallSource;

    use artcraft_launcher_core::protocol::LatestRelease;

    use super::*;
    use crate::source::Answer;

    /// GitHub, recording which repos it's asked about.
    #[derive(Default)]
    struct RecordingGitHub(Mutex<Vec<String>>);

    impl ReleaseSource for RecordingGitHub {
        fn latest(&self, repo: &str, _pre: bool) -> Answer {
            self.0.lock().unwrap().push(repo.to_string());
            Answer { latest: LatestRelease::None, source: Source::GitHubApi }
        }
        fn cached(&self, _repo: &str, _pre: bool) -> Option<LatestRelease> {
            None
        }
        fn rate_limited_until(&self) -> Option<i64> {
            None
        }
        fn save(&self) {}
    }

    fn shared() -> Shared {
        Shared {
            events: Mutex::new(Vec::new()),
            waker: Arc::new(|| {}),
            latest: Mutex::new(HashMap::new()),
            installed: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            manifest: Mutex::new(Manifest::default()),
            options: Mutex::new(BackendOptions::default()),
            checking: AtomicBool::new(false),
            detecting: AtomicBool::new(false),
            last_check: Mutex::new(None),
        }
    }

    const NOW: i64 = 1_791_500_000;

    fn feed() -> Feed {
        Feed::parse(
            r#"{"release_info": {"apps": {
            "photocraft": {"latest_release": {"tag": "v0.6.0"}, "app_last_fetched": "2026-10-09T13:00:00Z"}
        }}}"#,
        )
        .unwrap()
    }

    #[test]
    fn the_limit_notice_shows_only_while_details_are_missing() {
        let until = Some(NOW + 31 * 60);
        // GitHub said no earlier, but the API (or a local mirror) has since answered every app.
        assert_eq!(limit_notice(Source::Feed, until), None);
        assert_eq!(limit_notice(Source::GitHubApi, until), None);
        // Some apps came from the release pages or saved answers: say when the rest returns.
        assert_eq!(limit_notice(Source::GitHubPages, until), until);
        assert_eq!(limit_notice(Source::Cache, until), until);
        assert_eq!(limit_notice(Source::Cache, None), None);
    }

    #[test]
    fn a_local_mirror_answers_for_every_app_on_its_own() {
        let photo = CraftApp::by_id("photocraft").unwrap();
        let word = CraftApp::by_id("wordcraft").unwrap();
        let feed = feed().with_mirror("http://127.0.0.1:8787/");
        // Listed: found, even long after the sync.
        let (latest, _, _) = mirror_latest(photo, Some(&feed), NOW + 30 * 86_400);
        assert!(matches!(latest, Some(LatestRelease::Found(ref r)) if r.tag == "v0.6.0"));
        // Not listed: no release (rather than asking GitHub).
        assert!(matches!(mirror_latest(word, Some(&feed), NOW).0, Some(LatestRelease::None)));
        // Not running: says so.
        assert!(matches!(mirror_latest(photo, None, NOW).0, Some(LatestRelease::Failed(_))));
        assert!(is_loopback("http://localhost:8787/") && is_loopback("http://[::1]:8787/"));
        assert!(!is_loopback("http://192.168.1.2:8787/") && !is_loopback("http://localhost.evil.example/"));
        assert!(!is_loopback("http://127.0.0.1:1@evil.example/") && !is_loopback("http://127.0.0.1:8787@evil.example:80/"));
        assert!(!is_loopback("http://127.0.0.1/") && !is_loopback("http://127.0.0.1:x/") && !is_loopback("https://127.0.0.1:8787/"));
        assert!(is_loopback("http://127.0.0.1:8787") && is_loopback("http://127.0.0.1:8787/release_info.json"));
    }

    #[test]
    fn every_app_is_asked_about_at_its_own_repo_under_the_cinch_owner() {
        let github = RecordingGitHub::default();
        for app in &SUITE {
            let (_, from, used) = github_latest(app, &github, false);
            assert_eq!((from, used), (Origin::GitHub, Some(Source::GitHubApi)));
        }
        let asked = github.0.into_inner().unwrap();
        let expected: Vec<String> = SUITE.iter().map(|a| format!("{}/{}", artcraft_launcher_core::catalog::CINCH_OWNER, a.id)).collect();
        assert_eq!(asked, expected);
    }

    const APP: &str = "photocraft";

    /// A PhotoCraft release with one file per platform and format, so whichever this machine
    /// installs is there.
    fn release(version: &str) -> Release {
        let suffixes = ["-portable.zip", ".tar.gz", ".msi", ".dmg", ".AppImage", ".deb", ".rpm", ".flatpak"];
        let tokens =
            ["windows-x64", "windows-x86", "windows-arm64", "macos-universal", "linux-x86_64", "linux-aarch64", "linux-riscv64", "freebsd-x86_64"];
        let assets = tokens
            .iter()
            .flat_map(|t| suffixes.iter().map(move |s| format!("{APP}-{version}-{t}{s}")))
            .map(|name| Asset { url: format!("https://example.invalid/{name}"), name, size: 3, sha256: None })
            .collect();
        Release { assets, ..Release::default() }
    }

    /// A backend that knows `release("0.5.0")` and keeps its files under `root`.
    fn backend(root: &std::path::Path, platform: Platform, source: Option<Arc<dyn ReleaseSource>>) -> NativeBackend {
        let shared = shared();
        shared.latest.lock().unwrap().insert(APP.to_string(), release("0.5.0"));
        let shared = Arc::new(shared);
        NativeBackend { shared, paths: Paths::under(root), http: None, source, platform: Some(platform) }
    }

    /// Reports again, and returns what the UI was last told about a kept download.
    fn ready(backend: &mut NativeBackend) -> Option<bool> {
        let app = CraftApp::by_id(APP).unwrap();
        report_partial(app, &backend.shared, &backend.paths, backend.platform, &BackendOptions::default());
        last_ready(backend.poll())
    }

    fn last_ready(events: Vec<Event>) -> Option<bool> {
        events.into_iter().rev().find_map(|e| match e {
            Event::ReadyToInstall { app, ready } if app == APP => Some(ready),
            _ => None,
        })
    }

    /// Puts the planned download on disk, as a failed install leaves it.
    fn keep_file(backend: &NativeBackend) -> std::path::PathBuf {
        let app = CraftApp::by_id(APP).unwrap();
        let (dest, size) = planned_download(app, &backend.shared, &backend.paths, backend.platform, &BackendOptions::default()).unwrap();
        std::fs::create_dir_all(backend.paths.downloads()).unwrap();
        std::fs::write(&dest, vec![0u8; size as usize]).unwrap();
        dest
    }

    #[test]
    fn a_kept_download_is_reported_and_can_be_thrown_away() {
        let Some(platform) = native_platform() else { return };
        let dir = tempfile::tempdir().unwrap();
        let mut backend = backend(dir.path(), platform, None);
        assert_eq!(ready(&mut backend), Some(false));
        let dest = keep_file(&backend);
        assert_eq!(ready(&mut backend), Some(true));

        backend.send(Command::Cancel { app: APP.to_string() });
        assert!(!dest.exists());
        assert_eq!(last_ready(backend.poll()), Some(false));
    }

    #[test]
    fn changing_the_settings_reports_kept_downloads_again() {
        let Some(platform) = native_platform() else { return };
        let dir = tempfile::tempdir().unwrap();
        let mut backend = backend(dir.path(), platform, None);
        keep_file(&backend);
        assert_eq!(ready(&mut backend), Some(true));
        // The planned file changed under the UI; only a new report can tell it.
        backend.shared.latest.lock().unwrap().insert(APP.to_string(), release("0.6.0"));
        backend.send(Command::Configure(BackendOptions::default()));
        assert_eq!(last_ready(backend.poll()), Some(false));
    }

    #[test]
    fn a_check_that_finds_a_newer_release_ends_the_retry_offer() {
        struct Newer;
        impl ReleaseSource for Newer {
            fn latest(&self, repo: &str, _prereleases: bool) -> crate::source::Answer {
                let found = CraftApp::by_id(APP).is_some_and(|a| a.repo == repo);
                let latest = if found { LatestRelease::Found(release("0.6.0")) } else { LatestRelease::None };
                crate::source::Answer { latest, source: Source::GitHubApi }
            }
            fn cached(&self, _repo: &str, _prereleases: bool) -> Option<LatestRelease> {
                None
            }
            fn rate_limited_until(&self) -> Option<i64> {
                None
            }
            fn save(&self) {}
        }
        let Some(platform) = native_platform() else { return };
        let dir = tempfile::tempdir().unwrap();
        let mut backend = backend(dir.path(), platform, Some(Arc::new(Newer)));
        // A check needs a client, but asks only `Newer`: there is no other service to call.
        backend.http = Some(Http::new().unwrap());
        keep_file(&backend);
        assert_eq!(ready(&mut backend), Some(true));

        backend.send(Command::CheckForUpdates { force: true });
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !events.iter().any(|e| matches!(e, Event::CheckFinished { .. })) {
            assert!(Instant::now() < deadline, "the check never finished");
            std::thread::sleep(Duration::from_millis(10));
            events.extend(backend.poll());
        }
        assert_eq!(last_ready(events), Some(false));
    }

    fn copy(v: &str) -> Installed {
        Installed { version: version::parse(v), source: InstallSource::Deb, location: String::new(), package: None }
    }

    #[test]
    fn a_failed_job_counts_as_installed_only_when_the_new_version_arrived() {
        let latest = version::parse("0.5.0").unwrap();
        // One app of a shared installer call went in before another failed the call.
        assert!(landed(Some(&copy("0.4.1")), Some(&copy("0.5.0")), &latest));
        assert!(landed(None, Some(&copy("0.5.0")), &latest));
        // Nothing changed, so the error stands.
        assert!(!landed(Some(&copy("0.4.1")), Some(&copy("0.4.1")), &latest));
        assert!(!landed(None, None, &latest));
        // It was current before the job ran: the job did not install it.
        assert!(!landed(Some(&copy("0.5.0")), Some(&copy("0.5.0")), &latest));
        // A copy whose version can't be read proves nothing.
        let unreadable = Installed { version: None, ..copy("0.5.0") };
        assert!(!landed(Some(&copy("0.4.1")), Some(&unreadable), &latest));
    }

    #[test]
    fn only_a_failed_shared_call_can_count_as_installed() {
        let latest = version::parse("0.5.0").unwrap();
        let (before, after) = (Some(copy("0.4.1")), Some(copy("0.5.0")));
        let landed_after = |error: &str| landed_despite(error, before.as_ref(), after.as_ref(), &latest);
        // An app's own failure stands, even if its new version shows up (an MSI or AppImage that
        // reported an error, a cancelled or dismissed prompt, a pause).
        for error in ["the Windows Installer stopped with error 1603", "couldn't copy the AppImage", CANCELLED, INSTALL_CANCELLED, PAUSED] {
            assert!(!landed_after(error), "{error}");
        }
        // A call shared with other apps may have installed this one before it failed.
        let shared = "the system installer stopped with error 100 while handling 2 apps together. Try them one at a time.";
        assert_eq!(landed_after(shared), cfg!(target_os = "linux"));
        assert!(!landed_despite(shared, before.as_ref(), before.as_ref(), &latest));
    }

    #[test]
    fn self_update_is_off_and_the_launcher_repo_is_never_asked() {
        let Some(platform) = native_platform() else { return };
        let dir = tempfile::tempdir().unwrap();
        let github = Arc::new(RecordingGitHub::default());
        let mut backend = backend(dir.path(), platform, Some(github.clone()));
        assert!(!backend.capabilities().self_update);
        backend.send(Command::UpdateLauncher);
        assert!(backend.poll().iter().any(|e| matches!(e, Event::JobFinished { result: Err(_), .. })));
        backend.http = Some(Http::new().unwrap());
        backend.send(Command::CheckForUpdates { force: true });
        let deadline = Instant::now() + Duration::from_secs(20);
        while !backend.poll().iter().any(|e| matches!(e, Event::CheckFinished { .. })) {
            assert!(Instant::now() < deadline, "the check never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut asked = github.0.lock().unwrap().clone();
        asked.sort();
        let mut expected: Vec<String> = SUITE.iter().map(|a| a.repo.to_string()).collect();
        expected.sort();
        assert_eq!(asked, expected, "every app once, and nothing else");
    }
}
