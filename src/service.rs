//! Service lifecycle + unit quoting, split
//! by what this box actually supervises with. Python's `service.py` only ever
//! spoke to systemd; gray's own gateway service probes runit/systemd/none
//! (crates/gray/src/gateway/service.rs), and this plugin follows that path so
//! a runit or supervisor-less box is not stranded. Never places tokens in
//! unit files, run scripts, or logs.

use std::path::{Path, PathBuf};

/// Systemd user unit name (unchanged from parity).
pub const NAME: &str = "gray-slack-plugin.service";
/// Supervision name under runit and gray's own "none" supervisor (runit
/// service dirs are one word; the systemd unit keeps its long name).
pub const SVC: &str = "gray-slack";

/// What this box can supervise a daemon with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supervisor {
    /// runit/runsv user services (`$SVDIR`, `~/.config/service`, `/var/service`).
    Runit { dir: PathBuf },
    /// systemd user units (`$XDG_CONFIG_HOME/systemd/user`, …).
    SystemdUser { unit_dir: PathBuf },
    /// No init to lean on: gray spawns the daemon detached (pidfile + log).
    None,
}

/// User systemd directory (`$XDG_CONFIG_HOME` or `~/.config`).
pub fn unit_path() -> PathBuf {
    systemd_user_dir().join(NAME)
}

fn systemd_user_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("systemd/user");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config/systemd/user");
    }
    PathBuf::from(".config/systemd/user")
}

/// The runit service tree this user's services live in.
pub fn svdir() -> PathBuf {
    svdir_from(std::env::var_os("SVDIR"), std::env::var_os("HOME"))
}

/// `$SVDIR` wins, then `~/.config/service`, then the system tree (same order
/// as gray's gateway detection; split out so the resolution is testable).
pub fn svdir_from(svdir: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = svdir {
        return PathBuf::from(dir);
    }
    if let Some(home) = home {
        return PathBuf::from(home).join(".config/service");
    }
    PathBuf::from("/var/service")
}

fn proc_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn find_in_path(tool: &str) -> bool {
    if tool.contains('/') {
        return std::path::Path::new(tool).is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file())
}

/// Which supervisor this box actually has. PID 1 decides first; the tools on
/// PATH are the fallback, exactly like gray's gateway detection (a runit box
/// without `sv` in this process's PATH still is a runit box).
pub fn detect() -> Supervisor {
    match proc_comm(1).as_deref() {
        Some("runit") | Some("runsvdir") => {
            return Supervisor::Runit { dir: svdir() };
        }
        Some("systemd") if find_in_path("systemctl") => {
            return Supervisor::SystemdUser {
                unit_dir: systemd_user_dir(),
            };
        }
        _ => {}
    }
    if find_in_path("sv") {
        return Supervisor::Runit { dir: svdir() };
    }
    if find_in_path("systemctl") {
        return Supervisor::SystemdUser {
            unit_dir: systemd_user_dir(),
        };
    }
    Supervisor::None
}

/// Quote one argv word for systemd `ExecStart` (Python parity: backslash,
/// quote, `%`, `$` escaped; newline/CR/NUL rejected).
pub fn quote(value: &str) -> Result<String, String> {
    if value.contains(['\n', '\r', '\0']) {
        return Err("Invalid service path".to_string());
    }
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}

/// The daemon argv for this binary and config: `<exe> run --config <path>`.
fn daemon_argv(config_path: &Path) -> Result<Vec<String>, String> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|_| "cannot locate service binary".to_string())?;
    let resolved = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    Ok(vec![
        exe,
        "run".to_string(),
        "--config".to_string(),
        resolved,
    ])
}

/// Single-quote for `sh`, keeping embedded quotes intact (same discipline as
/// gray's `runit_script`).
fn sh_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The runit `run` script for the daemon: exec the argv, nothing else.
pub fn runit_script(argv: &[String]) -> String {
    let mut script = String::from("#!/bin/sh\nexec");
    for arg in argv {
        script.push(' ');
        script.push_str(&sh_quote(arg));
    }
    script.push('\n');
    script
}

/// Render the user unit. Argv uses the current Rust binary — replaces the
/// Re-exec equivalent of `python -m <module>`.
pub fn unit(config_path: &Path) -> Result<String, String> {
    let argv = daemon_argv(config_path)?;
    let mut exec: Vec<String> = Vec::with_capacity(argv.len());
    for w in &argv {
        exec.push(quote(w)?);
    }
    Ok(format!(
        "[Unit]\nDescription=gray Slack plugin\nAfter=network-online.target\n\
         [Service]\nType=simple\nExecStart={}\nRestart=on-failure\nRestartSec=10\n\
         UMask=0077\nKillMode=control-group\nTimeoutStopSec=15\n\
         [Install]\nWantedBy=default.target\n",
        exec.join(" ")
    ))
}

// ---------------------------------------------------------------------------
// systemd backend
// ---------------------------------------------------------------------------

/// Run `systemctl --user <args>`, capturing both streams.
fn systemd_run(args: &[&str]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("systemctl");
    cmd.arg("--user");
    for a in args {
        cmd.arg(a);
    }
    match cmd.output() {
        Ok(out) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let text = text.trim().to_string();
            if !out.status.success() {
                let mut msg = format!(
                    "systemctl {} failed: {}",
                    args.join(" "),
                    text.lines().last().unwrap_or("unknown error")
                );
                if unit_not_loaded(&text) {
                    msg.push_str(&format!(
                        " — {NAME} isn't loaded; 'systemctl --user daemon-reload' \
                         may pick it up, or 'gray {} install' recreates it",
                        SVC.strip_prefix("gray-").unwrap_or(SVC)
                    ));
                }
                return Err(msg);
            }
            Ok(text)
        }
        Err(e) => Err(format!("systemctl failed; check the user session: {e}")),
    }
}

/// Whether `systemctl --user` can see the unit right now. A unit file left
/// on disk by a failed install reads NotFound until a daemon-reload picks
/// it up — setup should offer a fresh install, not "restart".
enum UnitLoad {
    Loaded,
    NotFound,
    /// Can't ask (no systemctl, no user bus): let the lifecycle call
    /// surface the real error instead of guessing either way.
    Unknown,
}

fn unit_load_state() -> UnitLoad {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", NAME, "-p", "LoadState", "--value"])
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => match String::from_utf8_lossy(&o.stdout).trim() {
            "not-found" => UnitLoad::NotFound,
            _ => UnitLoad::Loaded,
        },
        Ok(o) if unit_not_loaded(&String::from_utf8_lossy(&o.stderr)) => UnitLoad::NotFound,
        _ => UnitLoad::Unknown,
    }
}

/// Does this systemctl output mean the manager can't see the unit (as
/// opposed to the call or the unit itself being broken)?
fn unit_not_loaded(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("could not be found")
        || t.contains("does not exist")
        || t.contains("not found")
        || t.contains("not-found")
        || t.contains("not loaded")
}

// ---------------------------------------------------------------------------
// runit backend
// ---------------------------------------------------------------------------

fn sv_run(svc: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("sv");
    cmd.arg("-w").arg("10");
    for a in args {
        cmd.arg(a);
    }
    cmd.arg(svc);
    match cmd.output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let text = text.trim().to_string();
            if !out.status.success() {
                return Err(format!(
                    "sv {} failed: {}",
                    args.join(" "),
                    text.lines().last().unwrap_or("unknown error")
                ));
            }
            Ok(text)
        }
        Err(e) => Err(format!("could not run sv: {e}")),
    }
}

/// `sv status` keeps its exit-code semantics (nonzero when down), so it
/// reports rather than errors.
/// Wait until a runsv supervisor owns this service dir (or the wait lapses).
/// `sv status` succeeding means the `supervise/` fifos exist — adoption.
fn wait_for_supervision(svc: &Path, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let adopted = std::process::Command::new("sv")
            .arg("status")
            .arg(svc)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if adopted {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(250).min(deadline - now));
    }
}

fn sv_status(svc: &Path) -> String {
    let mut cmd = std::process::Command::new("sv");
    cmd.arg("status");
    cmd.arg(svc);
    match cmd.output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            text.trim().to_string()
        }
        Err(e) => format!("could not run sv status: {e}"),
    }
}

// ---------------------------------------------------------------------------
// gray (no-init) supervision: detached spawn + pidfile + log
// ---------------------------------------------------------------------------

fn pidfile_path(config_dir: &Path) -> PathBuf {
    config_dir.join(format!("{SVC}.pid"))
}

fn log_path(config_dir: &Path) -> PathBuf {
    config_dir.join(format!("{SVC}.log"))
}

fn spawn_detached(argv: &[String], config_dir: &Path) -> Result<u32, String> {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(config_dir))
        .map_err(|e| format!("cannot open the daemon log: {e}"))?;
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone().map_err(|e| e.to_string())?)
        .stderr(log_file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(log_path(config_dir), std::fs::Permissions::from_mode(0o600));
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid in pre_exec only creates a session for the child;
        // it touches no memory.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: the daemon outlives
        // this console the way setsid detaches a unix child.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", argv[0]))?;
    let pid = child.id();
    std::fs::write(pidfile_path(config_dir), pid.to_string())
        .map_err(|e| format!("cannot record the pid file: {e}"))?;
    Ok(pid)
}

/// The pid a gray-supervised daemon recorded, and whether it is still alive.
fn running_pid(config_dir: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(pidfile_path(config_dir)).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    if pid_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) only probes.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0;
            let queried = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            // STILL_ACTIVE
            queried && code == 259
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn term_pid(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        // SAFETY: kill with a parsed pid has no memory-safety surface.
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            return Err(format!("could not signal pid {pid}"));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return Err(format!("could not signal pid {pid}"));
            }
            let ok = TerminateProcess(handle, 0) != 0;
            CloseHandle(handle);
            if ok {
                return Ok(());
            }
            return Err(format!("could not signal pid {pid}"));
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err("stopping a gray-supervised daemon is unsupported on this platform".to_string())
    }
}

fn config_dir_of(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

// ---------------------------------------------------------------------------
// public lifecycle (each split per supervisor; injected-supervisor for tests)
// ---------------------------------------------------------------------------

/// Install + start the daemon under whatever this box supervises with.
pub fn install(config_path: &Path) -> Result<String, String> {
    install_with(detect(), config_path)
}

pub fn status(config_path: &Path) -> Result<String, String> {
    status_with(detect(), config_path)
}

pub fn stop(config_path: &Path) -> Result<String, String> {
    // A marker left by a restart that never happened must not turn this
    // stop into "restarting".
    crate::lifecycle::clear_restart(&config_dir_of(config_path));
    stop_with(detect(), config_path)
}

pub fn restart(config_path: &Path) -> Result<String, String> {
    restart_with(detect(), config_path)
}

/// Disable + remove the service. Private configuration, sessions and the
/// registered outgoing tool are retained on purpose.
pub fn uninstall(config_path: &Path) -> Result<String, String> {
    uninstall_with(detect(), config_path)
}

/// Is the daemon already set up under this box's supervisor? Setup re-runs
/// restart it instead of asking to install again.
pub fn installed(config_path: &Path) -> bool {
    match detect() {
        Supervisor::Runit { dir } => dir.join(SVC).join("run").is_file(),
        Supervisor::SystemdUser { unit_dir } => {
            // A unit file systemd never loaded (aborted install, stale
            // leftover) isn't an installed service — setup should ask to
            // install again, not "restart" a unit that doesn't exist.
            unit_dir.join(NAME).exists() && !matches!(unit_load_state(), UnitLoad::NotFound)
        }
        Supervisor::None => running_pid(&config_dir_of(config_path)).is_some(),
    }
}

pub fn install_with(sup: Supervisor, config_path: &Path) -> Result<String, String> {
    let argv = daemon_argv(config_path)?;
    match sup {
        Supervisor::Runit { dir } => {
            let svc = dir.join(SVC);
            let run = svc.join("run");
            let body = runit_script(&argv);
            if run.exists() {
                let existing = std::fs::read_to_string(&run)
                    .map_err(|_| "cannot read the existing run script".to_string())?;
                if existing != body {
                    return Err(
                        "A different service already exists; uninstall it first".to_string()
                    );
                }
            }
            std::fs::create_dir_all(&svc)
                .map_err(|_| "cannot create the runit service directory".to_string())?;
            std::fs::write(&run, &body).map_err(|_| "cannot write the run script".to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755))
                    .map_err(|_| "cannot make the run script executable".to_string())?;
            }
            match sv_run(&svc, &["up"]) {
                Ok(_) => {}
                Err(first) => {
                    // runsvdir adopts new service dirs on its next scan
                    // (every few seconds), so `sv up` can lose that race and
                    // report a missing supervise/control while the daemon is
                    // already starting. Poll status before failing.
                    if !wait_for_supervision(&svc, std::time::Duration::from_secs(15)) {
                        return Err(first);
                    }
                }
            }
            Ok(format!("{SVC} runs under runit at {}", svc.display()))
        }
        Supervisor::SystemdUser { unit_dir } => {
            let target = unit_dir.join(NAME);
            let body = unit(config_path)?;
            if target.exists() {
                let existing =
                    std::fs::read_to_string(&target).map_err(|_| "cannot read unit".to_string())?;
                // A unit file the manager can't see guards nothing — a stale
                // install can always be rewritten. A *loaded* but different
                // unit still refuses until it is uninstalled.
                if existing != body && !matches!(unit_load_state(), UnitLoad::NotFound) {
                    return Err(
                        "A different plugin service already exists; uninstall it first".to_string(),
                    );
                }
            }
            if let Some(p) = target.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .create(p)
                    .map_err(|_| "cannot create systemd unit directory".to_string())?;
            }
            std::fs::write(&target, body).map_err(|_| "cannot write unit file".to_string())?;
            systemd_run(&["daemon-reload"])?;
            systemd_run(&["enable", "--now", NAME])?;
            Ok(format!(
                "{SVC} runs under systemd --user ({})",
                target.display()
            ))
        }
        Supervisor::None => {
            let dir = config_dir_of(config_path);
            if let Some(pid) = running_pid(&dir) {
                return Ok(format!("{SVC} already runs under gray (pid {pid})"));
            }
            let pid = spawn_detached(&argv, &dir)?;
            Ok(format!(
                "{SVC} runs under gray (pid {pid}, log {})",
                log_path(&dir).display()
            ))
        }
    }
}

pub fn status_with(sup: Supervisor, config_path: &Path) -> Result<String, String> {
    match sup {
        Supervisor::Runit { dir } => {
            let svc = dir.join(SVC);
            if !svc.join("run").is_file() {
                return Ok(format!(
                    "not installed (no runit service in {})",
                    dir.display()
                ));
            }
            Ok(format!(
                "{}\n(supervised by runit at {})",
                sv_status(&svc),
                svc.display()
            ))
        }
        Supervisor::SystemdUser { .. } => {
            if !unit_path().exists() {
                return Ok(format!(
                    "not installed (no unit at {})",
                    unit_path().display()
                ));
            }
            Ok(systemd_run(&["status", NAME])?)
        }
        Supervisor::None => {
            let dir = config_dir_of(config_path);
            match running_pid(&dir) {
                Some(pid) => Ok(format!(
                    "{SVC} runs under gray (pid {pid}, log {})",
                    log_path(&dir).display()
                )),
                None => {
                    if pidfile_path(&dir).exists() {
                        Ok(format!(
                            "not running (stale pid file at {}; start with gray slack install)",
                            pidfile_path(&dir).display()
                        ))
                    } else {
                        Ok(format!(
                            "not running under gray (no pid file at {}; start with gray slack install)",
                            pidfile_path(&dir).display()
                        ))
                    }
                }
            }
        }
    }
}

pub fn stop_with(sup: Supervisor, config_path: &Path) -> Result<String, String> {
    match sup {
        Supervisor::Runit { dir } => {
            let svc = dir.join(SVC);
            if !svc.join("run").is_file() {
                return Err(format!("no runit service at {} to stop", svc.display()));
            }
            sv_run(&svc, &["down"])?;
            Ok(format!("{SVC} stopped ({} is down)", svc.display()))
        }
        Supervisor::SystemdUser { .. } => {
            systemd_run(&["stop", NAME])?;
            Ok(format!("{SVC} stopped (systemd --user)"))
        }
        Supervisor::None => {
            let dir = config_dir_of(config_path);
            let text = std::fs::read_to_string(pidfile_path(&dir)).map_err(|_| {
                format!(
                    "no pid file at {} (not started by gray?)",
                    pidfile_path(&dir).display()
                )
            })?;
            let pid: u32 = text
                .trim()
                .parse()
                .map_err(|_| format!("unreadable pid file at {}", pidfile_path(&dir).display()))?;
            term_pid(pid)?;
            let _ = std::fs::remove_file(pidfile_path(&dir));
            Ok(format!("stopped {SVC} (pid {pid})"))
        }
    }
}

pub fn restart_with(sup: Supervisor, config_path: &Path) -> Result<String, String> {
    // Tells the old daemon its SIGTERM is a restart, so chats hear
    // "restarting" rather than "shutting down".
    crate::lifecycle::request_restart(&config_dir_of(config_path));
    match sup {
        Supervisor::Runit { dir } => {
            let svc = dir.join(SVC);
            if !svc.join("run").is_file() {
                return Err(format!("no runit service at {} to restart", svc.display()));
            }
            sv_run(&svc, &["restart"])?;
            Ok(format!("{SVC} restarted ({})", svc.display()))
        }
        Supervisor::SystemdUser { .. } => {
            systemd_run(&["restart", NAME])?;
            Ok(format!("{SVC} restarted (systemd --user)"))
        }
        Supervisor::None => {
            // No supervisor restarts anything: stop the old pid (if any),
            // spawn a fresh daemon.
            stop_with(Supervisor::None, config_path).ok();
            install_with(Supervisor::None, config_path)
        }
    }
}

pub fn uninstall_with(sup: Supervisor, config_path: &Path) -> Result<String, String> {
    match sup {
        Supervisor::Runit { dir } => {
            let svc = dir.join(SVC);
            if !svc.join("run").is_file() {
                return Ok(format!("no runit service at {} to remove", svc.display()));
            }
            sv_run(&svc, &["down"]).ok();
            std::fs::remove_dir_all(&svc)
                .map_err(|_| "cannot remove the runit service directory".to_string())?;
            Ok(format!("removed the runit service at {}", svc.display()))
        }
        Supervisor::SystemdUser { .. } => {
            let target = unit_path();
            if !target.exists() {
                return Ok(format!("no unit at {} to remove", target.display()));
            }
            // A stale unit file the manager never loaded can't be
            // disabled — remove it anyway or a failed install can never
            // be cleaned up.
            if let Err(e) = systemd_run(&["disable", "--now", NAME]) {
                if !matches!(unit_load_state(), UnitLoad::NotFound) {
                    return Err(e);
                }
            }
            let _ = std::fs::remove_file(&target);
            let _ = systemd_run(&["daemon-reload"]);
            Ok(format!("removed the systemd unit {}", target.display()))
        }
        Supervisor::None => {
            let dir = config_dir_of(config_path);
            let stopped = stop_with(Supervisor::None, config_path).is_ok();
            let log = log_path(&dir).display().to_string();
            if stopped {
                Ok(format!("stopped {SVC} (log kept at {log})"))
            } else {
                Ok(format!("no running daemon (log kept at {log})"))
            }
        }
    }
}
