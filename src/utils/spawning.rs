use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::{io, thread};

use libc::{RLIMIT_NOFILE, getrlimit, rlimit, setrlimit};
use niri_config::Environment;

use crate::utils::expand_home;

pub static REMOVE_ENV_RUST_BACKTRACE: AtomicBool = AtomicBool::new(false);
pub static REMOVE_ENV_RUST_LIB_BACKTRACE: AtomicBool = AtomicBool::new(false);
pub static CHILD_ENV: RwLock<Environment> = RwLock::new(Environment(Vec::new()));

static ORIGINAL_NOFILE_RLIMIT_CUR: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_NOFILE_RLIMIT_MAX: AtomicU64 = AtomicU64::new(0);

/// Increases the nofile rlimit to the maximum and stores the original value.
pub fn store_and_increase_nofile_rlimit() {
    let mut rlim = rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `&raw mut rlim` points at a valid, live `rlimit` for `getrlimit` to
    // write into.
    if unsafe { getrlimit(RLIMIT_NOFILE, &raw mut rlim) } != 0 {
        let err = io::Error::last_os_error();
        warn!("error getting nofile rlimit: {err:?}");
        return;
    }

    ORIGINAL_NOFILE_RLIMIT_CUR.store(rlim.rlim_cur, Ordering::SeqCst);
    ORIGINAL_NOFILE_RLIMIT_MAX.store(rlim.rlim_max, Ordering::SeqCst);

    trace!(
        "changing nofile rlimit from {} to {}",
        rlim.rlim_cur, rlim.rlim_max
    );
    rlim.rlim_cur = rlim.rlim_max;

    // SAFETY: `&raw const rlim` points at a valid, initialized `rlimit` for
    // `setrlimit` to read.
    if unsafe { setrlimit(RLIMIT_NOFILE, &raw const rlim) } != 0 {
        let err = io::Error::last_os_error();
        warn!("error setting nofile rlimit: {err:?}");
    }
}

/// Restores the original nofile rlimit.
pub fn restore_nofile_rlimit() {
    let rlim_cur = ORIGINAL_NOFILE_RLIMIT_CUR.load(Ordering::SeqCst);
    let rlim_max = ORIGINAL_NOFILE_RLIMIT_MAX.load(Ordering::SeqCst);

    if rlim_cur == 0 {
        return;
    }

    let rlim = rlimit { rlim_cur, rlim_max };
    // SAFETY: same as the `setrlimit` call in `store_and_increase_nofile_rlimit`.
    unsafe { setrlimit(RLIMIT_NOFILE, &raw const rlim) };
}

/// Spawns the command to run independently of the compositor.
///
/// # Panics
///
/// Does not panic: the early `command.is_empty()` return guarantees `split_first()`
/// always returns `Some` inside the spawned thread.
pub fn spawn<T: AsRef<OsStr> + Send + 'static>(command: Vec<T>) {
    if command.is_empty() {
        return;
    }

    // Spawning and waiting takes some milliseconds, so do it in a thread.
    let res = thread::Builder::new()
        .name("Command Spawner".to_owned())
        .spawn(move || {
            let (command, args) = command.split_first().unwrap();
            spawn_sync(command, args);
        });

    if let Err(err) = res {
        warn!("error spawning a thread to spawn the command: {err:?}");
    }
}

/// Spawns the command through the shell.
///
/// We hardcode `sh -c`, consistent with other compositors:
///
/// - <https://github.com/swaywm/sway/blob/b3dcde8d69c3f1304b076968a7a64f54d0c958be/sway/commands/exec_always.c#L64>
/// - <https://github.com/hyprwm/Hyprland/blob/1ac1ff457ab8ef1ae6a8f2ab17ee7965adfa729f/src/managers/KeybindManager.cpp#L987>
pub fn spawn_sh(command: String) {
    spawn(vec![String::from("sh"), String::from("-c"), command]);
}

fn spawn_sync(command: impl AsRef<OsStr>, args: impl IntoIterator<Item = impl AsRef<OsStr>>) {
    let mut command = command.as_ref();

    // Expand `~` at the start.
    let expanded = expand_home(Path::new(command));
    match &expanded {
        Ok(Some(expanded)) => command = expanded.as_ref(),
        Ok(None) => (),
        Err(err) => {
            warn!("error expanding ~: {err:?}");
        }
    }

    let mut process = Command::new(command);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Remove RUST_BACKTRACE and RUST_LIB_BACKTRACE from the environment if needed.
    if REMOVE_ENV_RUST_BACKTRACE.load(Ordering::Relaxed) {
        process.env_remove("RUST_BACKTRACE");
    }
    if REMOVE_ENV_RUST_LIB_BACKTRACE.load(Ordering::Relaxed) {
        process.env_remove("RUST_LIB_BACKTRACE");
    }

    // Remove the systemd NOTIFY_SOCKET variable.
    process.env_remove("NOTIFY_SOCKET");

    // Never leak a host X11 DISPLAY to children.
    process.env_remove("DISPLAY");

    // Set configured environment.
    let env = CHILD_ENV.read().unwrap();
    for var in &env.0 {
        if let Some(value) = &var.value {
            process.env(&var.name, value);
        } else {
            process.env_remove(&var.name);
        }
    }
    drop(env);

    // SAFETY: `pre_exec` runs the closure between fork and exec in the child, where
    // only async-signal-safe operations are allowed. `unblock_all` only calls
    // `pthread_sigmask`, which is async-signal-safe.
    unsafe { process.pre_exec(crate::utils::signals::unblock_all) };

    let Some(mut child) = do_spawn(command, process) else {
        return;
    };

    match child.wait() {
        Ok(status) => {
            if !status.success() {
                warn!("child did not exit successfully: {status:?}");
            }
        }
        Err(err) => {
            warn!("error waiting for child: {err:?}");
        }
    }
}

fn do_spawn(command: &OsStr, mut process: Command) -> Option<Child> {
    // SAFETY: `pre_exec` runs the closure between fork and exec in the child, where
    // only async-signal-safe operations are allowed. The closure below only calls
    // `fork()`, `_exit()`, and `restore_nofile_rlimit` (which only calls `setrlimit`),
    // all of which are async-signal-safe.
    unsafe {
        // Double-fork to avoid having to waitpid the child.
        process.pre_exec(move || {
            match libc::fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => (),
                _ => libc::_exit(0),
            }

            restore_nofile_rlimit();

            Ok(())
        });
    }

    let child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!("error spawning {command:?}: {err:?}");
            return None;
        }
    };

    Some(child)
}
