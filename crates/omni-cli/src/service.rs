//! Installing the daemon as a service: a systemd user unit, a launchd
//! LaunchAgent, or a per-user Windows logon task.
//!
//! `omni-sync daemon install` writes a platform service definition pointing at
//! the *absolute* path of this binary and the client config, then hands it to
//! the platform's service manager. Service managers never expand `~`, so every
//! path is resolved to an absolute one first.
//!
//! The generators ([`systemd_unit`], [`launchd_plist`], [`windows_task_xml`])
//! are pure string functions, so every platform's definition is unit-tested on
//! any host; only [`install`], [`uninstall`] and [`status`] touch the machine.
//! Generated files carry a `Managed by omni-sync` marker, so a re-install
//! overwrites our own file freely and only demands `--force` for a foreign one.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// Reverse-DNS service label, mirroring the `ProjectDirs` qualifier/org/app.
pub const LABEL: &str = "dev.albruiz.omni-sync";
/// systemd unit name.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const UNIT_NAME: &str = "omni-sync.service";
/// Windows Task Scheduler task name.
#[allow(dead_code)]
pub const TASK_NAME: &str = "omni-sync";
/// Marker written into every generated file so we can recognise our own work.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub const MARKER: &str = "Managed by omni-sync";

/// Options shared by every platform's `install`.
#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    /// Install a system-wide unit instead of a per-user one (systemd only).
    pub system: bool,
    /// Write and enable the service but do not start it now.
    pub no_start: bool,
    /// Overwrite a service file that omni-sync did not write.
    pub force: bool,
    /// Print the generated service definition and change nothing.
    pub dry_run: bool,
}

/// Everything a generated service definition needs. Every path is absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Absolute path to the `omni-sync` binary.
    pub exe: PathBuf,
    /// Absolute path to the client config.
    pub config: PathBuf,
    /// Reverse-DNS service label ([`LABEL`]).
    pub label: String,
    /// The invoking user (the `User=` of a system unit, the task's principal).
    pub user: String,
    /// The invoking user's home directory.
    pub home: PathBuf,
}

impl ServiceSpec {
    /// Resolve the running binary, config and user into an absolute spec.
    pub fn resolve(config: &Path) -> Result<Self> {
        let exe = absolute_exe()?;
        let config = std::path::absolute(config)
            .with_context(|| format!("resolving {}", config.display()))?;
        let home = directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        #[cfg(target_os = "windows")]
        let user = windows_user();
        #[cfg(not(target_os = "windows"))]
        let user = current_user();

        Ok(Self {
            exe,
            config,
            label: LABEL.to_string(),
            user,
            home,
        })
    }

    /// The daemon arguments the service runs (after the binary path).
    pub fn arguments(&self) -> Vec<String> {
        vec![
            "--config".to_string(),
            self.config.display().to_string(),
            "daemon".to_string(),
        ]
    }

    /// Where redirected stdout/stderr go on platforms without a journal.
    pub fn log_file(&self) -> PathBuf {
        self.home.join("Library/Logs/omni-sync.log")
    }
}

fn absolute_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running binary")?;
    Ok(strip_verbatim(exe))
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

fn current_user() -> String {
    std::env::var("SUDO_USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|user| !user.is_empty()))
        .or_else(|| {
            std::env::var("LOGNAME")
                .ok()
                .filter(|user| !user.is_empty())
        })
        .or_else(|| {
            std::env::var("USERNAME")
                .ok()
                .filter(|user| !user.is_empty())
        })
        .unwrap_or_else(|| "root".to_string())
}

#[cfg(target_os = "windows")]
fn windows_user() -> String {
    let user = current_user();
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    }
}

/// True when `exe` sits under a `target/debug` or `target/release` directory,
/// i.e. it is a transient cargo build rather than an installed binary.
pub fn looks_like_build_artifact(exe: &Path) -> bool {
    let components: Vec<&str> = exe
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect();
    components
        .windows(2)
        .any(|pair| pair[0] == "target" && (pair[1] == "debug" || pair[1] == "release"))
}

/// True when `text` is a file that omni-sync wrote (and may overwrite).
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub fn is_managed(text: &str) -> bool {
    text.contains(MARKER)
}

// --- pure generators -------------------------------------------------------

/// Quote a path or argument for a systemd `ExecStart` line. systemd does not
/// run a shell, but it does expand `%` specifiers and `$` environment
/// references, so both are escaped here as well.
#[allow(dead_code)]
pub fn systemd_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// A systemd unit. `system` targets `/etc/systemd/system` (runs as `User=`);
/// otherwise it is a per-user unit under `~/.config/systemd/user`.
#[allow(dead_code)]
pub fn systemd_unit(spec: &ServiceSpec, system: bool) -> String {
    let mut exec = systemd_quote(&spec.exe.display().to_string());
    for arg in spec.arguments() {
        exec.push(' ');
        exec.push_str(&systemd_quote(&arg));
    }
    let install_target = if system {
        "multi-user.target"
    } else {
        "default.target"
    };

    let mut unit = String::new();
    unit.push_str("# Managed by omni-sync; regenerate with `omni-sync daemon install`.\n");
    unit.push_str("[Unit]\n");
    unit.push_str("Description=omni-sync file sync daemon\n");
    unit.push_str("After=network-online.target\n");
    unit.push_str("Wants=network-online.target\n\n");
    unit.push_str("[Service]\n");
    unit.push_str("Type=simple\n");
    if system {
        unit.push_str(&format!("User={}\n", spec.user));
        unit.push_str(&format!(
            "Environment=HOME={}\n",
            systemd_quote(&spec.home.display().to_string())
        ));
    }
    unit.push_str("Environment=RUST_LOG=info\n");
    unit.push_str(&format!("ExecStart={exec}\n"));
    unit.push_str("Restart=on-failure\n");
    unit.push_str("RestartSec=5\n");
    unit.push_str("StartLimitIntervalSec=100\n");
    unit.push_str("StartLimitBurst=10\n\n");
    unit.push_str("[Install]\n");
    unit.push_str(&format!("WantedBy={install_target}\n"));
    unit
}

/// A launchd LaunchAgent property list.
#[allow(dead_code)]
pub fn launchd_plist(spec: &ServiceSpec) -> String {
    let mut args = vec![spec.exe.display().to_string()];
    args.extend(spec.arguments());
    let mut program = String::new();
    for arg in &args {
        program.push_str("\t\t<string>");
        program.push_str(&xml_escape(arg));
        program.push_str("</string>\n");
    }
    let log = xml_escape(&spec.log_file().display().to_string());

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Managed by omni-sync; regenerate with `omni-sync daemon install`. -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n{program}\t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<dict>\n\
         \t\t<key>SuccessfulExit</key>\n\
         \t\t<false/>\n\
         \t</dict>\n\
         \t<key>ProcessType</key>\n\
         \t<string>Background</string>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>{log}</string>\n\
         \t<key>EnvironmentVariables</key>\n\
         \t<dict>\n\
         \t\t<key>RUST_LOG</key>\n\
         \t\t<string>info</string>\n\
         \t</dict>\n\
         </dict>\n\
         </plist>\n",
        label = xml_escape(&spec.label),
        program = program,
        log = log,
    )
}

/// A Windows Task Scheduler definition: a per-user task triggered at logon.
#[allow(dead_code)]
pub fn windows_task_xml(spec: &ServiceSpec) -> String {
    let arguments = xml_escape(&windows_arguments(&spec.arguments()));
    let working_dir = spec
        .config
        .parent()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \t<RegistrationInfo>\n\
         \t\t<Description>Managed by omni-sync; regenerate with `omni-sync daemon install`.</Description>\n\
         \t</RegistrationInfo>\n\
         \t<Triggers>\n\
         \t\t<LogonTrigger>\n\
         \t\t\t<Enabled>true</Enabled>\n\
         \t\t</LogonTrigger>\n\
         \t</Triggers>\n\
         \t<Principals>\n\
         \t\t<Principal id=\"Author\">\n\
         \t\t\t<UserId>{user}</UserId>\n\
         \t\t\t<LogonType>InteractiveToken</LogonType>\n\
         \t\t\t<RunLevel>LeastPrivilege</RunLevel>\n\
         \t\t</Principal>\n\
         \t</Principals>\n\
         \t<Settings>\n\
         \t\t<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \t\t<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \t\t<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
         \t\t<AllowHardTerminate>true</AllowHardTerminate>\n\
         \t\t<StartWhenAvailable>true</StartWhenAvailable>\n\
         \t\t<RestartOnFailure>\n\
         \t\t\t<Interval>PT1M</Interval>\n\
         \t\t\t<Count>3</Count>\n\
         \t\t</RestartOnFailure>\n\
         \t\t<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
         \t\t<Enabled>true</Enabled>\n\
         \t\t<Hidden>false</Hidden>\n\
         \t</Settings>\n\
         \t<Actions Context=\"Author\">\n\
         \t\t<Exec>\n\
         \t\t\t<Command>{command}</Command>\n\
         \t\t\t<Arguments>{arguments}</Arguments>\n\
         \t\t\t<WorkingDirectory>{working_dir}</WorkingDirectory>\n\
         \t\t</Exec>\n\
         \t</Actions>\n\
         </Task>\n",
        user = xml_escape(&spec.user),
        command = xml_escape(&spec.exe.display().to_string()),
        arguments = arguments,
        working_dir = xml_escape(&working_dir),
    )
}

/// Encode the task XML as UTF-16LE with a byte-order mark, which is what
/// `schtasks /XML` expects.
#[allow(dead_code)]
pub fn encode_task_xml(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Join arguments into a Windows command line, quoting what needs it.
#[allow(dead_code)]
pub fn windows_arguments(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(' ') || arg.contains('"') {
                format!("\"{}\"", arg.replace('"', "\\\""))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `systemctl` arguments to restart the unit (`try-restart` is a no-op when the
/// unit is not running).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_restart_args() -> Vec<&'static str> {
    vec!["try-restart", UNIT_NAME]
}

/// `launchctl` arguments to kill and restart the agent.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launchd_kickstart_args(uid: u32) -> Vec<String> {
    vec![
        "kickstart".to_string(),
        "-k".to_string(),
        format!("gui/{uid}/{LABEL}"),
    ]
}

/// `schtasks` arguments to stop the task; it fails when the task is not running.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn schtasks_end_args() -> Vec<&'static str> {
    vec!["/End", "/TN", TASK_NAME]
}

/// `schtasks` arguments to start the task.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn schtasks_run_args() -> Vec<&'static str> {
    vec!["/Run", "/TN", TASK_NAME]
}

#[allow(dead_code)]
fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

// --- entry points ----------------------------------------------------------

/// `--system` only means something for systemd; fail rather than silently
/// install a per-user service on the other platforms.
fn ensure_system_supported(system: bool) -> Result<()> {
    if system && !cfg!(target_os = "linux") {
        bail!("--system applies to systemd units only; omit it on this platform");
    }
    Ok(())
}

/// Install and (unless `no_start`) start the daemon service.
pub fn install(config: &Path, options: &InstallOptions) -> Result<()> {
    ensure_system_supported(options.system)?;
    let spec = ServiceSpec::resolve(config)?;
    if !options.force && looks_like_build_artifact(&spec.exe) {
        bail!(
            "{} looks like a cargo build artifact; install a released binary first \
             (or pass --force)",
            spec.exe.display()
        );
    }
    install_platform(spec, options)
}

/// Stop and remove the installed service.
pub fn uninstall(config: &Path, system: bool) -> Result<()> {
    ensure_system_supported(system)?;
    let spec = ServiceSpec::resolve(config)?;
    uninstall_platform(spec, system)
}

/// Report whether the service is installed, enabled and running.
pub fn status(config: &Path, system: bool) -> Result<()> {
    ensure_system_supported(system)?;
    let spec = ServiceSpec::resolve(config)?;
    status_platform(spec, system)
}

/// What `omni-sync update` should do about the running service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartDecision {
    /// Restart it without asking.
    Restart,
    /// Ask the user (a service is installed and we are on a terminal).
    Prompt,
    /// Leave it alone and print the manual hint.
    Skip,
}

/// Decide whether `update` should restart the service. Pure, so the policy is
/// unit-tested without a terminal or a service manager.
pub fn restart_decision(
    explicit: Option<bool>,
    installed: bool,
    interactive: bool,
) -> RestartDecision {
    match explicit {
        Some(true) => RestartDecision::Restart,
        Some(false) => RestartDecision::Skip,
        None if installed && interactive => RestartDecision::Prompt,
        None => RestartDecision::Skip,
    }
}

/// The result of trying to restart the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartOutcome {
    /// No service is installed; nothing to restart.
    NotInstalled,
    /// Installed but not currently running, so it was left alone.
    NotRunning,
    /// Restarted the service so it picks up the new binary.
    Restarted,
}

/// Whether a service definition for this user exists.
pub fn is_installed() -> bool {
    is_installed_platform()
}

/// Restart the installed service so it runs a freshly installed binary.
///
/// Only restarts a service that is currently running: a service the user
/// stopped stays stopped, and will pick up the new binary when next started.
/// `system` forces the systemd scope on Linux; `None` auto-detects it.
pub fn restart(system: Option<bool>) -> Result<RestartOutcome> {
    if let Some(system) = system {
        ensure_system_supported(system)?;
    }
    restart_platform(system)
}

// --- platform dispatch -----------------------------------------------------

fn install_platform(spec: ServiceSpec, options: &InstallOptions) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        install_linux(spec, options)
    }
    #[cfg(target_os = "macos")]
    {
        install_macos(spec, options)
    }
    #[cfg(target_os = "windows")]
    {
        install_windows(spec, options)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (spec, options);
        bail!("installing the daemon as a service is not supported on this platform")
    }
}

fn uninstall_platform(spec: ServiceSpec, system: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        uninstall_linux(spec, system)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = system;
        uninstall_macos(spec)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (spec, system);
        uninstall_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (spec, system);
        bail!("uninstalling the daemon service is not supported on this platform")
    }
}

fn status_platform(spec: ServiceSpec, system: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        status_linux(spec, system)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = system;
        status_macos(spec)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (spec, system);
        status_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (spec, system);
        bail!("service status is not supported on this platform")
    }
}

fn is_installed_platform() -> bool {
    #[cfg(target_os = "linux")]
    {
        systemd_unit_present(true) || systemd_unit_present(false)
    }
    #[cfg(target_os = "macos")]
    {
        launchd_agent_path()
            .map(|path| path.exists())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        matches!(
            capture("schtasks", &["/Query", "/TN", TASK_NAME]),
            Some((true, _))
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

fn restart_platform(system: Option<bool>) -> Result<RestartOutcome> {
    #[cfg(target_os = "linux")]
    {
        restart_linux(system)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = system;
        restart_macos()
    }
    #[cfg(target_os = "windows")]
    {
        let _ = system;
        restart_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = system;
        Ok(RestartOutcome::NotInstalled)
    }
}

// --- shared I/O helpers ----------------------------------------------------

/// Write a generated service file atomically, refusing to clobber a foreign
/// file unless `force` is set.
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn write_service_file(path: &Path, contents: &str, force: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if let Ok(existing) = std::fs::read_to_string(path)
        && !is_managed(&existing)
        && !force
    {
        bail!(
            "{} already exists and was not written by omni-sync; pass --force to overwrite",
            path.display()
        );
    }

    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(name);

    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

/// Run a command, failing with its stderr when it exits non-zero.
fn run_ok(program: &str, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`{program} {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

/// Run a command and capture its output; `None` when the tool is missing.
fn capture(program: &str, args: &[&str]) -> Option<(bool, String)> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let text = if stdout.is_empty() && !stderr.is_empty() {
        stderr
    } else {
        stdout
    };
    Some((output.status.success(), text))
}

// --- Linux / systemd -------------------------------------------------------

#[cfg(target_os = "linux")]
fn systemd_path(system: bool) -> Result<PathBuf> {
    if system {
        Ok(PathBuf::from("/etc/systemd/system").join(UNIT_NAME))
    } else {
        let base = directories::BaseDirs::new()
            .map(|dirs| dirs.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(base.join("systemd/user").join(UNIT_NAME))
    }
}

#[cfg(target_os = "linux")]
fn home_of(user: &str) -> Option<PathBuf> {
    let text = capture("getent", &["passwd", user])?.1;
    let home = text.trim().split(':').nth(5)?;
    (!home.is_empty()).then(|| PathBuf::from(home))
}

#[cfg(target_os = "linux")]
fn linger_enabled(user: &str) -> bool {
    match capture("loginctl", &["show-user", user, "-p", "Linger", "--value"]) {
        Some((true, value)) => value.trim() == "yes",
        // Unknown (no logind, or the user is not known here): do not nag.
        _ => true,
    }
}

#[cfg(target_os = "linux")]
fn systemctl_scope(system: bool) -> &'static [&'static str] {
    if system { &[] } else { &["--user"] }
}

#[cfg(target_os = "linux")]
fn systemd_unit_present(system: bool) -> bool {
    systemd_path(system)
        .map(|path| path.exists())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn restart_linux(system: Option<bool>) -> Result<RestartOutcome> {
    let system = match system {
        Some(system) => system,
        None if systemd_unit_present(true) => true,
        None if systemd_unit_present(false) => false,
        None => return Ok(RestartOutcome::NotInstalled),
    };
    if !systemd_unit_present(system) {
        return Ok(RestartOutcome::NotInstalled);
    }

    let scope = systemctl_scope(system);
    // Leave a service the user stopped stopped; only a running one is restarted.
    let mut probe = scope.to_vec();
    probe.extend_from_slice(&["is-active", UNIT_NAME]);
    let running = capture("systemctl", &probe)
        .map(|(_, text)| text.trim() == "active")
        .unwrap_or(false);
    if !running {
        return Ok(RestartOutcome::NotRunning);
    }

    let mut args = scope.to_vec();
    args.extend(systemd_restart_args());
    run_ok("systemctl", &args)?;
    Ok(RestartOutcome::Restarted)
}

#[cfg(target_os = "linux")]
fn install_linux(mut spec: ServiceSpec, options: &InstallOptions) -> Result<()> {
    if options.system
        && let Some(home) = home_of(&spec.user)
    {
        spec.home = home;
    }

    let unit = systemd_unit(&spec, options.system);
    let path = systemd_path(options.system)?;

    if options.dry_run {
        print!("{unit}");
        println!("# would write {}", path.display());
        return Ok(());
    }

    write_service_file(&path, &unit, options.force)?;
    println!("wrote {}", path.display());

    let scope = systemctl_scope(options.system);
    let mut reload = scope.to_vec();
    reload.push("daemon-reload");
    run_ok("systemctl", &reload)?;

    let mut enable = scope.to_vec();
    enable.push("enable");
    if !options.no_start {
        enable.push("--now");
    }
    enable.push(UNIT_NAME);
    run_ok("systemctl", &enable)?;
    if options.no_start {
        println!("enabled {UNIT_NAME} (not started)");
    } else {
        println!("enabled and started {UNIT_NAME}");
    }

    if !options.system && !linger_enabled(&spec.user) {
        println!(
            "note: run `loginctl enable-linger {}` so the daemon starts at boot and \
             keeps running after logout",
            spec.user
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_linux(spec: ServiceSpec, system: bool) -> Result<()> {
    let path = systemd_path(system)?;
    let scope = systemctl_scope(system);

    let mut disable = scope.to_vec();
    disable.extend_from_slice(&["disable", "--now", UNIT_NAME]);
    if let Err(error) = run_ok("systemctl", &disable) {
        tracing::warn!(%error, "systemctl disable failed; removing the unit anyway");
    }

    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("removed {}", path.display());
    } else {
        println!("no unit at {}", path.display());
    }

    let mut reload = scope.to_vec();
    reload.push("daemon-reload");
    run_ok("systemctl", &reload)?;
    let _ = spec;
    Ok(())
}

#[cfg(target_os = "linux")]
fn status_linux(spec: ServiceSpec, system: bool) -> Result<()> {
    let path = systemd_path(system)?;
    println!("unit:      {}", path.display());
    println!("installed: {}", path.exists());

    let scope = systemctl_scope(system);
    for label in ["enabled", "running"] {
        let mut args = scope.to_vec();
        args.push(if label == "enabled" {
            "is-enabled"
        } else {
            "is-active"
        });
        args.push(UNIT_NAME);
        match capture("systemctl", &args) {
            Some((_, text)) if !text.is_empty() => println!("{label}:   {text}"),
            Some(_) => println!("{label}:   unknown"),
            None => println!("{label}:   unknown (systemctl unavailable)"),
        }
    }
    if !system && !linger_enabled(&spec.user) {
        println!(
            "linger:    disabled (run `loginctl enable-linger {}`)",
            spec.user
        );
    }
    Ok(())
}

// --- macOS / launchd -------------------------------------------------------

#[cfg(target_os = "macos")]
fn current_uid() -> Result<u32> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("running `id -u`")?;
    if !output.status.success() {
        bail!("`id -u` failed");
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("parsing `id -u` output")
}

#[cfg(target_os = "macos")]
fn launchd_path(spec: &ServiceSpec) -> PathBuf {
    spec.home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", spec.label))
}

/// The LaunchAgent path for the current user, without needing a config.
#[cfg(target_os = "macos")]
fn launchd_agent_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| {
        dirs.home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    })
}

#[cfg(target_os = "macos")]
fn restart_macos() -> Result<RestartOutcome> {
    if !launchd_agent_path()
        .map(|path| path.exists())
        .unwrap_or(false)
    {
        return Ok(RestartOutcome::NotInstalled);
    }

    let uid = current_uid()?;
    let label = format!("gui/{uid}/{LABEL}");
    let running = capture("launchctl", &["print", &label])
        .map(|(ok, text)| ok && text.lines().any(|line| line.trim() == "state = running"))
        .unwrap_or(false);
    if !running {
        return Ok(RestartOutcome::NotRunning);
    }

    let args = launchd_kickstart_args(uid);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ok("launchctl", &refs)?;
    Ok(RestartOutcome::Restarted)
}

#[cfg(target_os = "macos")]
fn install_macos(spec: ServiceSpec, options: &InstallOptions) -> Result<()> {
    let plist = launchd_plist(&spec);
    let path = launchd_path(&spec);

    if options.dry_run {
        print!("{plist}");
        println!("<!-- would write {} -->", path.display());
        return Ok(());
    }

    write_service_file(&path, &plist, options.force)?;
    println!("wrote {}", path.display());

    if options.no_start {
        println!(
            "not started; the agent loads at next login (or run `launchctl bootstrap gui/$(id -u) {}`)",
            path.display()
        );
        return Ok(());
    }

    let domain = format!("gui/{}", current_uid()?);
    // A previous instance may be loaded; ignore its absence.
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{}", spec.label)])
        .output();
    run_ok(
        "launchctl",
        &["bootstrap", &domain, &path.display().to_string()],
    )?;
    println!("loaded and started {}", spec.label);
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_macos(spec: ServiceSpec) -> Result<()> {
    let path = launchd_path(&spec);
    let domain = format!("gui/{}", current_uid()?);
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{}", spec.label)])
        .output();

    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("removed {}", path.display());
    } else {
        println!("no agent at {}", path.display());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn status_macos(spec: ServiceSpec) -> Result<()> {
    let path = launchd_path(&spec);
    println!("agent:     {}", path.display());
    println!("installed: {}", path.exists());

    let domain = format!("gui/{}", current_uid()?);
    let label = format!("{domain}/{}", spec.label);
    match capture("launchctl", &["print", &label]) {
        Some((true, text)) if !text.is_empty() => {
            let state = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("state = "))
                .unwrap_or("loaded");
            println!("running:   {state}");
        }
        Some((false, _)) => println!("running:   not loaded"),
        Some(_) => println!("running:   loaded"),
        None => println!("running:   unknown (launchctl unavailable)"),
    }
    Ok(())
}

// --- Windows / Task Scheduler ----------------------------------------------

#[cfg(target_os = "windows")]
fn task_file() -> PathBuf {
    std::env::temp_dir().join("omni-sync-task.xml")
}

#[cfg(target_os = "windows")]
fn install_windows(spec: ServiceSpec, options: &InstallOptions) -> Result<()> {
    let xml = windows_task_xml(&spec);

    if options.dry_run {
        print!("{xml}");
        return Ok(());
    }

    let temp = task_file();
    std::fs::write(&temp, encode_task_xml(&xml))
        .with_context(|| format!("writing {}", temp.display()))?;
    let temp_arg = temp.display().to_string();
    let result = run_ok(
        "schtasks",
        &["/Create", "/TN", TASK_NAME, "/XML", &temp_arg, "/F"],
    );
    let _ = std::fs::remove_file(&temp);
    result?;
    println!("registered task {}", TASK_NAME);

    if options.no_start {
        println!("not started; the task runs at next logon");
    } else {
        run_ok("schtasks", &["/Run", "/TN", TASK_NAME])?;
        println!("started task {TASK_NAME}");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_windows() -> Result<()> {
    run_ok("schtasks", &["/Delete", "/TN", TASK_NAME, "/F"])?;
    println!("deleted task {TASK_NAME}");
    Ok(())
}

#[cfg(target_os = "windows")]
fn restart_windows() -> Result<RestartOutcome> {
    if !matches!(
        capture("schtasks", &["/Query", "/TN", TASK_NAME]),
        Some((true, _))
    ) {
        return Ok(RestartOutcome::NotInstalled);
    }

    // `/End` fails when the task is not running, so it doubles as the probe:
    // only a task that was running is started again.
    let running = matches!(capture("schtasks", &schtasks_end_args()), Some((true, _)));
    if !running {
        return Ok(RestartOutcome::NotRunning);
    }
    run_ok("schtasks", &schtasks_run_args())?;
    Ok(RestartOutcome::Restarted)
}

#[cfg(target_os = "windows")]
fn status_windows() -> Result<()> {
    match capture("schtasks", &["/Query", "/TN", TASK_NAME]) {
        Some((true, text)) => println!("{text}"),
        Some((false, _)) => println!("task {TASK_NAME}: not installed"),
        None => println!("task {TASK_NAME}: unknown (schtasks unavailable)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            exe: PathBuf::from("/usr/local/bin/omni-sync"),
            config: PathBuf::from("/home/alice/.config/omni-sync/client.toml"),
            label: LABEL.to_string(),
            user: "alice".to_string(),
            home: PathBuf::from("/home/alice"),
        }
    }

    #[test]
    fn systemd_user_unit_runs_the_daemon_with_the_absolute_config() {
        let unit = systemd_unit(&spec(), false);
        assert!(unit.contains("Managed by omni-sync"));
        assert!(unit.contains(
            "ExecStart=\"/usr/local/bin/omni-sync\" \"--config\" \
             \"/home/alice/.config/omni-sync/client.toml\" \"daemon\""
        ));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("User="));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn systemd_system_unit_sets_the_user_and_home() {
        let unit = systemd_unit(&spec(), true);
        assert!(unit.contains("User=alice\n"));
        assert!(unit.contains("Environment=HOME=\"/home/alice\"\n"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn systemd_quoting_escapes_spaces_specifiers_and_dollars() {
        assert_eq!(systemd_quote("/a b"), "\"/a b\"");
        // A literal `%` would otherwise be read as a specifier, `$` as a var.
        assert_eq!(systemd_quote("/x%y$z"), "\"/x%%y$$z\"");
        assert_eq!(systemd_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn launchd_plist_lists_arguments_and_keeps_the_agent_alive() {
        let plist = launchd_plist(&spec());
        assert!(plist.contains("<key>ProgramArguments</key>"));
        assert!(plist.contains("<string>/usr/local/bin/omni-sync</string>"));
        assert!(plist.contains("<string>--config</string>"));
        assert!(plist.contains("<string>/home/alice/.config/omni-sync/client.toml</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>StandardErrorPath</key>"));
    }

    #[test]
    fn xml_escaping_protects_plist_and_task_text() {
        let mut spec = spec();
        spec.config = PathBuf::from("/home/a & b/client<>.toml");
        let plist = launchd_plist(&spec);
        assert!(plist.contains("a &amp; b"));
        assert!(!plist.contains("a & b"));
        assert!(plist.contains("&lt;&gt;"));
    }

    #[test]
    fn windows_task_triggers_at_logon_and_restarts_on_failure() {
        let xml = windows_task_xml(&spec());
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<UserId>alice</UserId>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<Command>/usr/local/bin/omni-sync</Command>"));
    }

    #[test]
    fn windows_arguments_quote_paths_with_spaces() {
        let args = vec![
            "--config".to_string(),
            "C:\\Users\\a b\\client.toml".to_string(),
            "daemon".to_string(),
        ];
        assert_eq!(
            windows_arguments(&args),
            "--config \"C:\\Users\\a b\\client.toml\" daemon"
        );
    }

    #[test]
    fn task_xml_encoding_is_utf16le_with_a_bom() {
        let bytes = encode_task_xml("<Task/>");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        assert_eq!(&bytes[2..4], &0x3Cu16.to_le_bytes());
        assert_eq!(bytes.len(), 2 + "<Task/>".encode_utf16().count() * 2);
    }

    #[test]
    fn only_our_own_files_are_managed() {
        assert!(is_managed("# Managed by omni-sync; regenerate"));
        assert!(is_managed("<!-- Managed by omni-sync; regenerate -->"));
        assert!(!is_managed("[Unit]\nDescription=custom\n"));
    }

    #[test]
    fn restart_decision_follows_the_flags_and_context() {
        use RestartDecision::{Prompt, Restart, Skip};
        // An explicit flag always wins.
        assert_eq!(restart_decision(Some(true), false, false), Restart);
        assert_eq!(restart_decision(Some(false), true, true), Skip);
        // Otherwise prompt only on a terminal with a service installed.
        assert_eq!(restart_decision(None, true, true), Prompt);
        assert_eq!(restart_decision(None, true, false), Skip);
        assert_eq!(restart_decision(None, false, true), Skip);
        assert_eq!(restart_decision(None, false, false), Skip);
    }

    #[test]
    fn restart_commands_match_each_service_manager() {
        assert_eq!(systemd_restart_args(), vec!["try-restart", UNIT_NAME]);
        assert_eq!(
            launchd_kickstart_args(501),
            vec!["kickstart", "-k", "gui/501/dev.albruiz.omni-sync"]
        );
        assert_eq!(schtasks_end_args(), vec!["/End", "/TN", TASK_NAME]);
        assert_eq!(schtasks_run_args(), vec!["/Run", "/TN", TASK_NAME]);
    }

    #[test]
    fn build_artifacts_are_refused() {
        assert!(looks_like_build_artifact(Path::new(
            "/home/a/omni-sync/target/debug/omni-sync"
        )));
        assert!(looks_like_build_artifact(Path::new(
            "/home/a/omni-sync/target/release/omni-sync"
        )));
        assert!(!looks_like_build_artifact(Path::new(
            "/usr/local/bin/omni-sync"
        )));
        assert!(!looks_like_build_artifact(Path::new(
            "/home/a/targeted/omni-sync"
        )));
    }
}
