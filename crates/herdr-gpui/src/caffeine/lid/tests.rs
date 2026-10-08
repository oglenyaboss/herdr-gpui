#![allow(clippy::unwrap_used)]

use super::*;
use gpui::TestAppContext;
use std::{cell::RefCell, path::Path, rc::Rc};

/// Every fake step appends its name to `log`, so tests read the order back.
fn fakes(
    dir: &Path,
    held_elsewhere: &str,
    disable_sleep: &str,
    restore_sleep: &str,
    install_rule: &str,
) -> Commands {
    let at = |script: &str| format!("{{ cd '{}' && {script}; }}", dir.display());
    Commands {
        held_elsewhere: at(held_elsewhere),
        disable_sleep: at(disable_sleep),
        restore_sleep: at(restore_sleep),
        install_rule: vec!["/bin/sh".into(), "-c".into(), at(install_rule)],
    }
}

fn working(dir: &Path) -> Commands {
    fakes(
        dir,
        "false",
        "echo hold >> log",
        "echo release >> log",
        "echo install >> log",
    )
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default()
}

type Reported = Rc<RefCell<Vec<Failure>>>;

fn install(commands: Commands, cx: &mut TestAppContext) -> Reported {
    cx.update(|cx| cx.default_global::<Caffeine>().lid.commands = commands);
    Rc::default()
}

fn want_now(wanted: bool, reported: &Reported, cx: &mut TestAppContext) {
    let reported = reported.clone();
    cx.update(|cx| {
        want(
            wanted,
            move |failure, _| reported.borrow_mut().push(failure),
            cx,
        )
    });
}

fn held(cx: &mut TestAppContext) -> bool {
    cx.read_global::<Caffeine, _>(|caffeine, _| caffeine.lid.held())
}

#[gpui::test]
fn holds_behind_a_watcher_and_releases_when_the_cup_turns_off(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(working(dir.path()), cx);

    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(held(cx));
    assert_eq!(log(dir.path()), "hold\n");

    want_now(false, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    assert_eq!(log(dir.path()), "hold\nrelease\n");
    assert!(reported.borrow().is_empty());
}

#[gpui::test]
fn installs_the_rule_only_when_sudo_refuses(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(
        fakes(
            dir.path(),
            "false",
            "test -e rule && echo hold >> log",
            "echo release >> log",
            "touch rule && echo install >> log",
        ),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(held(cx));
    assert_eq!(log(dir.path()), "install\nhold\n");

    want_now(false, &reported, cx);
    cx.run_until_parked();
    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(held(cx));
    assert_eq!(log(dir.path()), "install\nhold\nrelease\nhold\n");
    want_now(false, &reported, cx);
    cx.run_until_parked();
}

#[gpui::test]
fn a_cancelled_password_dialog_reports_and_is_not_retried(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(
        fakes(
            dir.path(),
            "false",
            "exit 1",
            "echo release >> log",
            "echo 'User canceled. (-128)' >&2; exit 1",
        ),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    assert!(!cx.read_global::<Caffeine, _>(|caffeine, _| caffeine.lid.wanted));
    // Sleep was never disabled, so there was nothing to restore.
    assert_eq!(log(dir.path()), "");
    let reported = reported.borrow();
    assert!(matches!(
        reported.as_slice(),
        [Failure::Hold(Error::LidPasswordCancelled)]
    ));
}

#[gpui::test]
fn other_install_failures_keep_their_detail(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(
        fakes(
            dir.path(),
            "false",
            "exit 1",
            "true",
            "echo 'visudo: syntax error' >&2; exit 1",
        ),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    let reported = reported.borrow();
    assert!(matches!(
        reported.as_slice(),
        [Failure::Hold(Error::LidCommandFailed { operation: "allow Herdr to change lid sleep", detail, .. })]
            if detail == "visudo: syntax error"
    ));
}

#[gpui::test]
fn a_failed_restore_tells_the_user_how_to_finish_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(
        fakes(dir.path(), "false", "echo hold >> log", "exit 3", "exit 1"),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    want_now(false, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    let reported = reported.borrow();
    assert!(matches!(
        reported.as_slice(),
        [Failure::Release(error @ Error::LidRelease(status))]
            if status.code() == Some(3) && error.to_string().contains("sudo pmset disablesleep 0")
    ));
}

#[gpui::test]
fn a_change_while_a_command_runs_settles_on_the_latest_wish(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(working(dir.path()), cx);

    want_now(true, &reported, cx);
    want_now(false, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    assert_eq!(log(dir.path()), "hold\nrelease\n");
    assert!(reported.borrow().is_empty());
}

#[gpui::test]
fn a_failed_hold_keeps_the_command_detail(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(
        fakes(
            dir.path(),
            "false",
            "test -e rule || exit 1; echo 'pmset: denied' >&2; exit 2",
            "true",
            "touch rule",
        ),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    let reported = reported.borrow();
    assert!(matches!(
        reported.as_slice(),
        [Failure::Hold(Error::LidCommandFailed { operation: "keep the Mac running with the lid closed", status, detail })]
            if status.code() == Some(2) && detail == "pmset: denied"
    ));
}

#[gpui::test]
fn sleep_disabled_by_another_holder_is_left_alone(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(
        fakes(
            dir.path(),
            "true",
            "echo hold >> log",
            "echo release >> log",
            "echo install >> log",
        ),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    assert_eq!(log(dir.path()), "");
    let reported = reported.borrow();
    assert!(matches!(
        reported.as_slice(),
        [Failure::Hold(Error::LidHeldElsewhere)]
    ));
}

/// The owner can die while the disable still runs; the watcher must restore
/// after it, not before.
#[test]
fn an_owner_gone_mid_disable_still_restores_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let commands = fakes(
        dir.path(),
        "false",
        "while [ ! -e go ]; do sleep 0.01; done; echo hold >> log",
        "echo release >> log",
        "true",
    );
    let mut watcher = command(&commands.watcher())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(watcher.stdin.take());
    drop(watcher.stdout.take());
    drop(watcher.stderr.take());
    std::fs::write(dir.path().join("go"), "").unwrap();

    assert!(watcher.wait().unwrap().success());
    assert_eq!(log(dir.path()), "hold\nrelease\n");
}

#[test]
fn the_rule_names_only_the_two_pmset_commands_by_uid() {
    assert_eq!(
        rule(501),
        "#501 ALL=(root) NOPASSWD: /usr/bin/pmset disablesleep 1, /usr/bin/pmset disablesleep 0"
    );
    let commands = Commands::default();
    assert_eq!(
        commands.disable_sleep,
        "/usr/bin/sudo -n /usr/bin/pmset disablesleep 1"
    );
    assert_eq!(
        commands.restore_sleep,
        "/usr/bin/sudo -n /usr/bin/pmset disablesleep 0"
    );
}

#[test]
fn each_user_gets_their_own_rule_file() {
    assert_eq!(rule_path(501), "/etc/sudoers.d/herdr-gpui-501");
    assert_ne!(rule_path(501), rule_path(502));
    let install = &Commands::default().install_rule[2];
    assert!(install.contains(&format!("> {}", rule_path(uid()))));
}
