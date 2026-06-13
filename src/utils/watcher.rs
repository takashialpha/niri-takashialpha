//! File modification watcher.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};
use std::{io, thread};

use niri_config::{Config, ConfigParseResult, ConfigPath};
use smithay::reexports::calloop::channel::SyncSender;

use crate::niri::State;

const POLLING_INTERVAL: Duration = Duration::from_millis(500);

pub struct Watcher {
    load_config: mpsc::Sender<Option<String>>,
}

struct WatcherInner {
    /// The paths we're watching.
    path: ConfigPath,

    /// Last observed props of the watched file.
    last_props: Option<Props>,

    /// Last observed props for included files.
    includes: HashMap<PathBuf, Option<Props>>,
}

/// Properties of the watched file.
///
/// Equality on this means the file did not change.
#[derive(Debug, PartialEq, Eq)]
struct Props {
    /// Modification time of the watched file.
    mtime: SystemTime,

    /// Canonical form of the watched path.
    ///
    /// We store the absolute path in addition to mtime to account for symlinked configs where the
    /// symlink target may change without mtime. This is common on nix where everything is a
    /// symlink to /nix/store, which keeps no mtime (= 1970-01-01).
    canonical: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
enum CheckResult {
    Missing,
    Unchanged,
    Changed,
}

impl Watcher {
    pub fn new(
        path: ConfigPath,
        includes: Vec<PathBuf>,
        mut process: impl FnMut(&ConfigPath) -> ConfigParseResult<Config, ()> + Send + 'static,
        changed: SyncSender<Result<Config, ()>>,
    ) -> Self {
        let (load_config, load_config_rx) = mpsc::channel();

        thread::Builder::new()
            .name(format!("Filesystem Watcher for {path:?}"))
            .spawn(move || {
                let mut inner = WatcherInner::new(path, includes);

                loop {
                    let mut should_load = match load_config_rx.recv_timeout(POLLING_INTERVAL) {
                        Ok(path) => {
                            if let Some(path) = path {
                                inner = WatcherInner::new(
                                    ConfigPath::Explicit(PathBuf::from(path)),
                                    Vec::new(),
                                );
                            }
                            true
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => false,
                    };

                    match inner.check() {
                        CheckResult::Missing => continue,
                        CheckResult::Unchanged => (),
                        CheckResult::Changed => {
                            trace!("config file changed");
                            should_load = true;
                        }
                    }

                    if should_load {
                        let res = process(&inner.path);

                        if let Err(err) = changed.send(res.config) {
                            warn!("error sending change notification: {err:?}");
                            break;
                        }

                        // There's a bit of time here between reading the config and reading
                        // properties of included files where an included file could change and
                        // remain unnoticed by the watcher. Not sure there's any good way around it
                        // though since we don't know the final set of includes until the config is
                        // parsed.
                        inner.set_includes(res.includes);
                    }
                }

                debug!("exiting watcher thread for {:?}", inner.path);
            })
            .unwrap();

        Self { load_config }
    }

    pub fn load_config(&self, path: Option<String>) {
        let _ = self.load_config.send(path);
    }
}

impl Props {
    fn from_path(path: &Path) -> io::Result<Self> {
        let canonical = path.canonicalize()?;
        let mtime = canonical.metadata()?.modified()?;
        Ok(Self { mtime, canonical })
    }

    fn from_config_path(config_path: &ConfigPath) -> io::Result<Self> {
        match config_path {
            ConfigPath::Explicit(path) => Self::from_path(path),
            ConfigPath::Regular {
                user_path,
                system_path,
            } => Self::from_path(user_path).or_else(|_| Self::from_path(system_path)),
        }
    }
}

impl WatcherInner {
    pub fn new(path: ConfigPath, includes: Vec<PathBuf>) -> Self {
        let last_props = Props::from_config_path(&path).ok();

        let mut rv = Self {
            path,
            last_props,
            includes: HashMap::new(),
        };
        rv.set_includes(includes);
        rv
    }

    pub fn check(&mut self) -> CheckResult {
        if let Ok(new_props) = Props::from_config_path(&self.path) {
            if self.last_props.as_ref() != Some(&new_props) {
                self.last_props = Some(new_props);
                CheckResult::Changed
            } else {
                for (path, last_props) in &mut self.includes {
                    let new_props = Props::from_path(path).ok();

                    // If an include goes missing while the main config file is unchanged, we
                    // consider that a change and reload.
                    if *last_props != new_props {
                        return CheckResult::Changed;
                    }
                }

                CheckResult::Unchanged
            }
        } else {
            CheckResult::Missing
        }
    }

    fn set_includes(&mut self, includes: Vec<PathBuf>) {
        self.includes = includes
            .into_iter()
            .map(|path| {
                let props = Props::from_path(&path).ok();
                (path, props)
            })
            .collect();
    }
}

pub fn setup(state: &mut State, config_path: &ConfigPath, includes: Vec<PathBuf>) {
    // Parsing the config actually takes > 20 ms on my beefy machine, so let's do it on the
    // watcher thread.
    let process = |path: &ConfigPath| {
        path.load().map_config_res(|res| {
            res.map_err(|err| {
                warn!("{err:?}");
            })
        })
    };

    let (tx, rx) = calloop::channel::sync_channel(1);
    state
        .niri
        .event_loop
        .insert_source(
            rx,
            |event: calloop::channel::Event<Result<Config, ()>>, _, state| match event {
                calloop::channel::Event::Msg(config) => {
                    let failed = config.is_err();
                    state.reload_config(config);
                    state.ipc_config_loaded(failed);
                }
                calloop::channel::Event::Closed => (),
            },
        )
        .unwrap();

    let watcher = Watcher::new(config_path.clone(), includes, process, tx);
    state.niri.config_file_watcher = Some(watcher);
}
