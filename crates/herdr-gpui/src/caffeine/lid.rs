//! Keeps the Mac running with its lid closed while the cup is on.
//!
//! Only `pmset disablesleep` overrides lid sleep, and it needs root, so a
//! sudoers rule limited to `pmset disablesleep 1` and `0` is installed once
//! through the system's own password dialog. The override outlives this
//! process, so a watcher child owns both changing and undoing it: it runs
//! `pmset disablesleep 1`, then `pmset disablesleep 0` once its stdin closes.
//! Turning the cup off closes it, and so do quitting and crashing, because the
//! kernel closes the pipe with the process.
//!
//! The setting is one flag for the whole Mac, so a hold is refused while
//! anything else already has sleep disabled; restoring it would end that
//! other holder's override.

use super::Caffeine;
use crate::{Error, Result};
use gpui::App;
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
};

/// Shown by the system's password dialog when the rule is installed.
const PASSWORD_PROMPT: &str = "Herdr wants to keep this Mac running with the lid closed.";
/// The watcher's line once sleep is disabled.
const HELD: &str = "held";
/// The watcher's exit status when sleep was already disabled.
const HELD_ELSEWHERE: i32 = 75;
/// Failure diagnostics keep the start of the command's stderr, bounded.
const DETAIL_LIMIT: usize = 200;

#[derive(Default)]
pub(super) struct Lid {
    /// Whether the cup is on with the closed-lid preference set.
    wanted: bool,
    state: State,
    commands: Commands,
}

/// A failed step: a refused hold means the preference no longer describes
/// what the Mac does, so the caller turns it off.
pub(super) enum Failure {
    Hold(Error),
    Release(Error),
}

impl Lid {
    pub(super) fn held(&self) -> bool {
        matches!(self.state, State::Held(_))
    }
}

#[derive(Default)]
enum State {
    #[default]
    Allowed,
    /// A command runs in the background; `sync` resumes once it settles.
    Busy,
    /// Sleep is disabled, and the watcher undoes it when its stdin closes.
    Held(Child),
}

/// The shell steps the watcher runs and the rule installer, so tests
/// substitute harmless ones.
#[derive(Clone)]
struct Commands {
    held_elsewhere: String,
    disable_sleep: String,
    restore_sleep: String,
    install_rule: Vec<String>,
}

impl Default for Commands {
    fn default() -> Self {
        let path = rule_path(uid());
        let install = format!(
            "printf '%s\\n' '{rule}' > {path} && chmod 0440 {path} \
             && /usr/sbin/visudo -c -f {path} || {{ rm -f {path}; exit 1; }}",
            rule = rule(uid()),
        );
        Self {
            held_elsewhere: "/usr/bin/pmset -g | /usr/bin/grep -q '^ *SleepDisabled[[:space:]]*1'"
                .into(),
            disable_sleep: "/usr/bin/sudo -n /usr/bin/pmset disablesleep 1".into(),
            restore_sleep: "/usr/bin/sudo -n /usr/bin/pmset disablesleep 0".into(),
            install_rule: vec![
                "/usr/bin/osascript".into(),
                "-e".into(),
                format!(
                    "do shell script {install:?} with administrator privileges with prompt {PASSWORD_PROMPT:?}"
                ),
            ],
        }
    }
}

impl Commands {
    /// Signals are ignored so only the closed pipe ends the wait, even when a
    /// terminal or logout signals the whole group. The handshake is an
    /// external `echo`, so if this process is already gone, the broken pipe
    /// fails only that `echo` and the restore still runs.
    fn watcher(&self) -> Vec<String> {
        let Self {
            held_elsewhere,
            disable_sleep,
            restore_sleep,
            ..
        } = self;
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "trap '' HUP INT TERM\n\
                 if {held_elsewhere}; then exit {HELD_ELSEWHERE}; fi\n\
                 {disable_sleep} || exit\n\
                 /bin/echo {HELD}\n\
                 exec >/dev/null 2>&1\n\
                 read line\n\
                 {restore_sleep}"
            ),
        ]
    }
}

/// Records whether sleep should stay disabled, and starts moving toward it.
pub(super) fn want(
    wanted: bool,
    report: impl Fn(Failure, &mut App) + Clone + 'static,
    cx: &mut App,
) {
    cx.default_global::<Caffeine>().lid.wanted = wanted;
    sync(report, cx);
}

/// Runs at most one command at a time; whatever changed meanwhile is picked
/// up when it settles.
fn sync(report: impl Fn(Failure, &mut App) + Clone + 'static, cx: &mut App) {
    let lid = &mut cx.default_global::<Caffeine>().lid;
    let commands = lid.commands.clone();
    let work = match (std::mem::take(&mut lid.state), lid.wanted) {
        (State::Allowed, true) => cx
            .background_executor()
            .spawn(async move { hold(&commands).map(Some).map_err(Failure::Hold) }),
        (State::Held(watcher), false) => cx
            .background_executor()
            .spawn(async move { release(watcher).map(|()| None).map_err(Failure::Release) }),
        (state, _) => {
            lid.state = state;
            return;
        }
    };
    cx.default_global::<Caffeine>().lid.state = State::Busy;
    cx.spawn(async move |cx| {
        let outcome = work.await;
        cx.update(|cx| {
            let lid = &mut cx.default_global::<Caffeine>().lid;
            let failure = match outcome {
                Ok(Some(watcher)) => {
                    lid.state = State::Held(watcher);
                    None
                }
                Ok(None) => {
                    lid.state = State::Allowed;
                    None
                }
                Err(failure) => {
                    // A refused hold is not retried until the cup or the
                    // preference changes again.
                    lid.state = State::Allowed;
                    if matches!(failure, Failure::Hold(_)) {
                        lid.wanted = false;
                    }
                    Some(failure)
                }
            };
            cx.refresh_windows();
            if let Some(failure) = failure {
                report.clone()(failure, cx);
            }
            sync(report, cx);
        });
    })
    .detach();
}

/// Disables sleep, installing the sudoers rule first if `sudo` refuses.
fn hold(commands: &Commands) -> Result<Child> {
    match start(commands) {
        Err(Error::LidCommandFailed { .. }) => {
            run(&commands.install_rule, "allow Herdr to change lid sleep").map_err(cancelled)?;
            start(commands)
        }
        held => held,
    }
}

/// Starts a watcher and waits until it has disabled sleep. The watcher runs
/// the disable itself, so a crash at any point still restores sleep after it.
fn start(commands: &Commands) -> Result<Child> {
    let operation = "keep the Mac running with the lid closed";
    let mut watcher = command(&commands.watcher())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::LidCommand { operation, source })?;
    let mut line = String::new();
    if let Some(stdout) = watcher.stdout.take() {
        BufReader::new(stdout)
            .read_line(&mut line)
            .map_err(|source| Error::LidCommand { operation, source })?;
    }
    if line.trim_end() == HELD {
        return Ok(watcher);
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = watcher.stderr.take() {
        // Only the start of the output is kept, so unreadable bytes do not
        // need reporting.
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = watcher
        .wait()
        .map_err(|source| Error::LidCommand { operation, source })?;
    if status.code() == Some(HELD_ELSEWHERE) {
        return Err(Error::LidHeldElsewhere);
    }
    Err(Error::LidCommandFailed {
        operation,
        status,
        detail: stderr.trim().chars().take(DETAIL_LIMIT).collect(),
    })
}

/// The password dialog's Cancel button exits `osascript` with AppleScript's
/// userCanceledErr, -128; anything else stays a command failure.
fn cancelled(error: Error) -> Error {
    match error {
        Error::LidCommandFailed { ref detail, .. } if detail.ends_with("(-128)") => {
            Error::LidPasswordCancelled
        }
        error => error,
    }
}

/// Closes the watcher's stdin and waits for its `pmset disablesleep 0`.
fn release(mut watcher: Child) -> Result<()> {
    drop(watcher.stdin.take());
    let status = watcher.wait().map_err(|source| Error::LidCommand {
        operation: "wait for the closed-lid watcher",
        source,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::LidRelease(status))
    }
}

fn run(argv: &[String], operation: &'static str) -> Result<()> {
    let output = command(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| Error::LidCommand { operation, source })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(Error::LidCommandFailed {
        operation,
        status: output.status,
        detail: stderr.trim().chars().take(DETAIL_LIMIT).collect(),
    })
}

fn command(argv: &[String]) -> Command {
    let mut command = Command::new(argv.first().map_or("", String::as_str));
    command.args(argv.iter().skip(1));
    command
}

/// One file per user, so another user's install leaves this one's rule.
fn rule_path(uid: u32) -> String {
    format!("/etc/sudoers.d/herdr-gpui-{uid}")
}

/// The sudoers entry, keyed by uid so no user name needs escaping; sudoers
/// reads `#501` as uid 501, not as a comment.
fn rule(uid: u32) -> String {
    format!(
        "#{uid} ALL=(root) NOPASSWD: /usr/bin/pmset disablesleep 1, /usr/bin/pmset disablesleep 0"
    )
}

#[cfg(unix)]
fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// The cup is macOS-only, so no rule is ever installed elsewhere.
#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

#[cfg(all(test, unix))]
mod tests;
