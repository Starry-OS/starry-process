use std::sync::Arc;

use starry_process::init_proc;

mod common;
use common::ProcessExt;

#[test]
fn child() {
    let parent = init_proc();
    let child = parent.new_child();
    assert!(Arc::ptr_eq(&parent, &child.parent().unwrap()));
    assert!(parent.children().iter().any(|c| Arc::ptr_eq(c, &child)));
}

#[test]
fn exit() {
    let parent = init_proc();
    let child = parent.new_child();
    child.exit();
    assert!(child.is_zombie());
    // Normal exit with code 0 -> status 0
    assert_eq!(child.wait_status(), Some(0));
    assert!(parent.children().iter().any(|c| Arc::ptr_eq(c, &child)));
}

#[test]
#[should_panic]
fn free_not_zombie() {
    init_proc().new_child().free();
}

#[test]
fn free() {
    let parent = init_proc().new_child();
    let child = parent.new_child();
    child.exit();
    child.free();
    assert!(parent.children().is_empty());
}

#[test]
fn reap() {
    let init = init_proc();

    let parent = init.new_child();
    let child = parent.new_child();

    parent.exit();
    assert!(Arc::ptr_eq(&init, &child.parent().unwrap()));
}

#[test]
fn thread_exit() {
    let parent = init_proc();
    let child = parent.new_child();

    child.add_thread(101);
    child.add_thread(102);

    let mut threads = child.threads();
    threads.sort();
    assert_eq!(threads, vec![101, 102]);

    let last = child.exit_thread(101, 7);
    assert!(!last);
    assert_eq!(child.exit_code(), 7);

    child.group_exit();
    assert!(child.is_group_exited());

    let last2 = child.exit_thread(102, 3);
    assert!(last2);
    child.exit();
    assert_eq!(child.exit_code(), 7);
    // Exit code 7 -> status (7 << 8) = 0x0700
    assert_eq!(child.wait_status(), Some(0x0700));
}

#[test]
fn test_stop_continue_integration() {
    let parent = init_proc();
    let child = parent.new_child();

    // Initial state
    assert!(!child.is_stopped());
    assert!(!child.is_continued());

    // Stop the process
    let sig_stop = 19; // SIGSTOP
    child.stop_by_signal(sig_stop);

    assert!(child.is_stopped());
    assert!(child.is_signal_stopped());
    assert!(!child.is_ptrace_stopped());

    // Consume stopped event
    // POSIX stopped status: (sig << 8) | 0x7f
    assert_eq!(child.wait_status(), Some((sig_stop << 8) | 0x7f));
    assert_eq!(child.try_consume_stopped(), Some(sig_stop));

    // Should be consumed now (still stopped, but event consumed)
    assert!(child.is_stopped());
    assert_eq!(child.try_consume_stopped(), None);

    // Continue the process
    child.continue_from_stop();

    assert!(!child.is_stopped());
    assert!(child.is_continued());

    // Consume continued event
    // POSIX continued status: 0xffff
    assert_eq!(child.wait_status(), Some(0xffff));
    assert!(child.try_consume_continued());

    // Should be consumed now
    assert!(!child.try_consume_continued());
}

#[test]
fn test_ptrace_integration() {
    let parent = init_proc();
    let child = parent.new_child();

    // Ptrace stop
    let sig_trap = 5; // SIGTRAP
    child.set_ptrace_stopped(sig_trap);

    assert!(child.is_stopped());
    assert!(child.is_ptrace_stopped());
    assert!(!child.is_signal_stopped());

    // Ptrace stops are NOT consumed by standard waitpid (try_consume_stopped)
    assert_eq!(child.try_consume_stopped(), None);

    // Resume from ptrace
    child.resume_from_ptrace_stop();

    assert!(!child.is_stopped());
    assert!(!child.is_ptrace_stopped());

    // Resume from ptrace does NOT set continued flag
    assert!(!child.is_continued());
}

#[test]
fn test_exit_signal_integration() {
    let parent = init_proc();
    let child = parent.new_child();

    let sig_kill = 9; // SIGKILL
    child.exit_with_signal(sig_kill, false);

    assert!(child.is_zombie());

    let info = child.get_zombie_info().expect("Should have zombie info");
    assert_eq!(info.signal, Some(sig_kill));
    assert_eq!(info.exit_code, 0); // Exit code is 0 for signal termination
    assert!(!info.core_dumped);

    // Verify POSIX wait status manufacturing
    let status = child.wait_status().expect("Should have wait status");
    assert_eq!(status & 0x7f, sig_kill);
    assert_eq!(status & 0x80, 0); // No core dump
}
