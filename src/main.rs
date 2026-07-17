#[macro_use]
extern crate tracing;

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::{env, mem};

use calloop::EventLoop;
use clap::Parser;
use niri::cli::{Cli, Sub};
use niri::ipc::client::handle_msg;
use niri::niri::State;
use niri::utils::spawning::{
    CHILD_ENV, REMOVE_ENV_RUST_BACKTRACE, REMOVE_ENV_RUST_LIB_BACKTRACE, spawn, spawn_sh,
    store_and_increase_nofile_rlimit,
};
use niri::utils::{version, watcher};
use niri_config::{Config, ConfigPath};
use niri_ipc::socket::SOCKET_PATH_ENV;
use smithay::reexports::wayland_server::Display;
use tracing_subscriber::EnvFilter;
use xdg::BaseDirectories;

const DEFAULT_LOG_FILTER: &str = "niri=debug,smithay::backend::renderer::gles=error";

// This is a single linear startup sequence (logging, CLI parsing, config loading, backend
// and event loop setup, spawning autostart commands); splitting it into helpers would just
// mean passing a long list of partially-built state between calls, which reads less clearly
// than the current top-to-bottom flow. Same reasoning as `Niri::new` in niri.rs.
#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set backtrace defaults if not set.
    if env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: this is the first statement in `main`, on the only thread that
        // exists so far, so no other code can be concurrently reading/writing env vars.
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
        REMOVE_ENV_RUST_BACKTRACE.store(true, Ordering::Relaxed);
    }
    if env::var_os("RUST_LIB_BACKTRACE").is_none() {
        // SAFETY: same as above, still single-threaded startup.
        unsafe { env::set_var("RUST_LIB_BACKTRACE", "0") };
        REMOVE_ENV_RUST_LIB_BACKTRACE.store(true, Ordering::Relaxed);
    }

    let directives = env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_LOG_FILTER.to_owned());
    let env_filter = EnvFilter::builder().parse_lossy(directives);
    tracing_subscriber::fmt()
        .compact()
        .with_writer(io::stderr)
        .with_env_filter(env_filter)
        .with_ansi_sanitization(false)
        .init();

    let cli = Cli::parse();

    if cli.session {
        // Set the current desktop for xdg-desktop-portal.
        // SAFETY: still single-threaded startup, before the event loop or any child
        // process is spawned.
        unsafe { env::set_var("XDG_CURRENT_DESKTOP", "niri") };
        // Ensure the session type is set to Wayland for xdg-autostart and Qt apps.
        // SAFETY: same as above.
        unsafe { env::set_var("XDG_SESSION_TYPE", "wayland") };
    }

    // Handle subcommands.
    if let Some(subcommand) = cli.subcommand {
        match subcommand {
            Sub::Validate { config } => {
                config_path(config).load().config?;
                info!("config is valid");
                return Ok(());
            }
            Sub::Msg { msg, json } => {
                handle_msg(msg, json)?;
                return Ok(());
            }
        }
    }

    // Block signals early so the masking is inherited by all threads spawned later.
    niri::utils::signals::block_early().unwrap();

    info!("starting niri-takashialpha commit {}", &version());

    // Load the config.
    let config_path = config_path(cli.config);
    // SAFETY: still single-threaded startup; `NIRI_CONFIG` has already been read via
    // `env_config_path` above and must not leak into spawned children.
    unsafe { env::remove_var("NIRI_CONFIG") };
    let (config_created_at, config_load_result) = config_path.load_or_create();
    let config_errored = config_load_result.config.is_err();
    let mut config = config_load_result.config.unwrap_or_else(|err| {
        warn!("{err:?}");
        Config::load_default()
    });
    let config_includes = config_load_result.includes;

    let spawn_at_startup = mem::take(&mut config.spawn_at_startup);
    let spawn_sh_at_startup = mem::take(&mut config.spawn_sh_at_startup);
    *CHILD_ENV.write().unwrap() = mem::take(&mut config.environment);

    store_and_increase_nofile_rlimit();

    // Create the main event loop.
    let mut event_loop = EventLoop::<State>::try_new().unwrap();

    // Handle Ctrl+C and other signals.
    niri::utils::signals::listen(&event_loop.handle());

    // Create the compositor.
    let display = Display::new().unwrap();

    // Increase the buffer size so that it's harder to crash a frozen client with a 1000 Hz mouse.
    set_default_max_buffer_size(&display, 1024 * 1024);

    let mut state = State::new(
        config,
        event_loop.handle(),
        event_loop.get_signal(),
        display,
        false,
        true,
    )
    .unwrap();

    // Set WAYLAND_DISPLAY for children.
    let socket_name = state.niri.socket_name.as_deref().unwrap();
    // SAFETY: no user commands have been spawned yet (that happens further down via
    // `spawn`/`spawn_sh`), and no other niri code reads/writes env vars concurrently
    // with the main thread at this point in startup.
    unsafe { env::set_var("WAYLAND_DISPLAY", socket_name) };
    info!(
        "listening on Wayland socket: {}",
        socket_name.to_string_lossy()
    );

    // Set NIRI_SOCKET for children.
    if let Some(ipc) = &state.niri.ipc_server {
        let socket_path = ipc.socket_path.as_deref().unwrap();
        // SAFETY: same as the `WAYLAND_DISPLAY` set_var above.
        unsafe { env::set_var(SOCKET_PATH_ENV, socket_path) };
        info!("IPC listening on: {}", socket_path.to_string_lossy());
    }

    // Avoid spawning children in the host X11.
    // SAFETY: same as the `WAYLAND_DISPLAY` set_var above; still before any child
    // process is spawned.
    unsafe { env::remove_var("DISPLAY") };

    if cli.session {
        // We're starting as a session. Import our variables.
        import_environment();
    }

    if env::var_os("NIRI_DISABLE_SYSTEM_MANAGER_NOTIFY").is_none_or(|x| x != "1") {
        // Send ready notification to the NOTIFY_FD file descriptor.
        if let Err(err) = notify_fd() {
            warn!("error notifying fd: {err:?}");
        }
    }

    watcher::setup(&mut state, &config_path, config_includes);

    // Spawn commands from cli and auto-start.
    spawn(cli.command);

    for elem in spawn_at_startup {
        spawn(elem.command);
    }
    for elem in spawn_sh_at_startup {
        spawn_sh(elem.command);
    }

    // Show the config error notification right away if needed.
    if config_errored {
        state.niri.config_error_notification.show();
        state.ipc_config_loaded(true);
    } else if let Some(path) = config_created_at {
        state.niri.config_error_notification.show_created(path);
    }

    // Run the compositor.
    event_loop
        .run(
            None,
            &mut state,
            niri::niri::State::refresh_and_flush_clients,
        )
        .unwrap();

    Ok(())
}

fn import_environment() {
    let variables = [
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        SOCKET_PATH_ENV,
    ]
    .join(" ");

    let rv = Command::new("/bin/sh")
        .args([
            "-c",
            &format!(
                "hash dbus-update-activation-environment 2>/dev/null && \
                 dbus-update-activation-environment {variables}"
            ),
        ])
        .spawn();
    // Wait for the import process to complete, otherwise services will start too fast without
    // environment variables available.
    match rv {
        Ok(mut child) => match child.wait() {
            Ok(status) => {
                if !status.success() {
                    warn!("import environment shell exited with {status}");
                }
            }
            Err(err) => {
                warn!("error waiting for import environment shell: {err:?}");
            }
        },
        Err(err) => {
            warn!("error spawning shell to import environment: {err:?}");
        }
    }
}

fn env_config_path() -> Option<PathBuf> {
    env::var_os("NIRI_CONFIG")
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
}

fn default_config_path() -> Option<PathBuf> {
    let Some(mut path) = BaseDirectories::with_prefix("niri").get_config_home() else {
        warn!("error retrieving config home directory");
        return None;
    };

    path.push("config.kdl");
    Some(path)
}

fn system_config_path() -> PathBuf {
    PathBuf::from("/etc/niri/config.kdl")
}

fn config_path(cli_path: Option<PathBuf>) -> ConfigPath {
    if let Some(explicit) = cli_path.or_else(env_config_path) {
        return ConfigPath::Explicit(explicit);
    }

    let system_path = system_config_path();

    if let Some(user_path) = default_config_path() {
        ConfigPath::Regular {
            user_path,
            system_path,
        }
    } else {
        // Couldn't find the home directory, or whatever.
        ConfigPath::Explicit(system_path)
    }
}

fn notify_fd() -> anyhow::Result<()> {
    let fd = match env::var("NOTIFY_FD") {
        Ok(notify_fd) => notify_fd.parse()?,
        Err(env::VarError::NotPresent) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    // SAFETY: still single-threaded, right at the start of `main`'s post-parse setup.
    unsafe { env::remove_var("NOTIFY_FD") };
    // SAFETY: `fd` was just parsed from `NOTIFY_FD`, which the service manager sets to
    // an open fd it hands off exclusively to this process; nothing else in niri owns it.
    let mut notif = unsafe { File::from_raw_fd(fd) };
    notif.write_all(b"READY=1\n")?;
    Ok(())
}

// The wayland-server crate has set_default_max_buffer_size() under a libwayland_1_23 feature, but
// this hard-requires libwayland-server >= 1.23 which is not present on e.g. Ubuntu 24.04. Since
// calling this is an optional enhancement, do it optionally at runtime.
fn set_default_max_buffer_size(display: &Display<State>, size: usize) {
    use std::ffi::c_void;

    // SAFETY: `dlopen`/`dlsym`/`dlclose` are called with valid null-terminated C
    // strings; `dlopen` is checked for a null return before any symbol lookup; the
    // `transmute` below only runs after `dlsym` succeeded and matches the documented
    // C signature of `wl_display_set_default_max_buffer_size`; `display_ptr` comes
    // from a live `Display<State>` handle.
    unsafe {
        // RTLD_NOLOAD ensures we only get a handle to the libwayland-server that wayland-rs has
        // already loaded into this process, rather than potentially pulling in a different copy.
        let lib = libc::dlopen(
            c"libwayland-server.so.0".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_NOLOAD,
        );
        if lib.is_null() {
            // It's not really expected that this can happen, maybe if some distro changes the
            // library name?
            warn!("cannot set default max buffer size: libwayland-server.so.0 is not loaded");
            return;
        }

        let sym = libc::dlsym(lib, c"wl_display_set_default_max_buffer_size".as_ptr());
        if sym.is_null() {
            // Expected on libwayland-server < 1.23.
            trace!("wl_display_set_default_max_buffer_size is missing; skipping");
        } else {
            let func: unsafe extern "C" fn(*mut c_void, libc::size_t) = std::mem::transmute(sym);
            let display_ptr = display.handle().backend_handle().display_ptr();
            func(display_ptr.cast(), size);
        }

        libc::dlclose(lib);
    }
}
