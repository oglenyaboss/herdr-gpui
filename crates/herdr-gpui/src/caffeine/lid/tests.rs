#![allow(clippy::unwrap_used)]

use super::*;
use gpui::TestAppContext;
use std::{cell::RefCell, path::Path, rc::Rc};

/// Every fake step appends its name to `log`, so tests read the order back.
fn shell(script: String) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script]
}

fn fakes(dir: &Path, disable_sleep: &str, install_rule: &str, watcher: &str) -> Commands {
    let at = |script: &str| shell(format!("cd '{}' && {script}", dir.display()));
    Commands {
        disable_sleep: at(disable_sleep),
        install_rule: at(install_rule),
        watcher: at(watcher),
    }
}

fn working(dir: &Path) -> Commands {
    fakes(
        dir,
        "echo hold >> log",
        "echo install >> log",
        "read line; echo release >> log",
    )
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default()
}

type Reported = Rc<RefCell<Vec<Error>>>;

fn install(commands: Commands, cx: &mut TestAppContext) -> Reported {
    cx.update(|cx| cx.default_global::<Caffeine>().lid.commands = commands);
    Rc::default()
}

fn want_now(wanted: bool, reported: &Reported, cx: &mut TestAppContext) {
    let reported = reported.clone();
    cx.update(|cx| {
        want(
            wanted,
            move |error, _| reported.borrow_mut().push(error),
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
            "test -e rule && echo hold >> log",
            "touch rule && echo install >> log",
            "read line; echo release >> log",
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
            "exit 1",
            "echo 'User canceled. (-128)' >&2; exit 1",
            "read line; echo release >> log",
        ),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    assert!(!cx.read_global::<Caffeine, _>(|caffeine, _| caffeine.lid.wanted));
    // The watcher still ran its restore, which is harmless here.
    assert_eq!(log(dir.path()), "release\n");
    let reported = reported.borrow();
    assert!(matches!(
        reported.as_slice(),
        [Error::LidCommandFailed { operation: "allow Herdr to change lid sleep", detail, .. }]
            if detail == "User canceled. (-128)"
    ));
}

#[gpui::test]
fn a_failed_restore_tells_the_user_how_to_finish_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let reported = install(
        fakes(
            dir.path(),
            "echo hold >> log",
            "exit 1",
            "read line; exit 3",
        ),
        cx,
    );

    want_now(true, &reported, cx);
    cx.run_until_parked();
    want_now(false, &reported, cx);
    cx.run_until_parked();
    assert!(!held(cx));
    let reported = reported.borrow();
    assert!(matches!(reported.as_slice(), [Error::LidRelease(status)] if status.code() == Some(3)));
    assert!(
        reported[0]
            .to_string()
            .contains("sudo pmset disablesleep 0")
    );
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

#[test]
fn the_rule_names_only_the_two_pmset_commands_by_uid() {
    assert_eq!(
        rule(501),
        "#501 ALL=(root) NOPASSWD: /usr/bin/pmset disablesleep 1, /usr/bin/pmset disablesleep 0"
    );
    let commands = Commands::default();
    assert_eq!(
        commands.disable_sleep,
        ["/usr/bin/sudo", "-n", "/usr/bin/pmset", "disablesleep", "1"]
    );
    assert!(commands.watcher[2].ends_with("/usr/bin/sudo -n /usr/bin/pmset disablesleep 0"));
}
