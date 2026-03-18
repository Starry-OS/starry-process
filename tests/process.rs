use std::{
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};

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
    child.transition_to_zombie();
    assert!(child.is_zombie());
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
    child.transition_to_zombie();
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
    assert_eq!(child.exit_code(), 7);
}

#[test]
fn state_lifecycle() {
    let process = init_proc().new_child();

    // Initial state should be RUNNING
    assert!(process.is_running());
    assert!(!process.is_stopped());
    assert!(!process.is_zombie());

    // RUNNING -> STOPPED
    process.transition_to_stopped();
    assert!(!process.is_running());
    assert!(process.is_stopped());
    assert!(!process.is_zombie());

    // STOPPED -> RUNNING
    process.transition_to_running();
    assert!(process.is_running());
    assert!(!process.is_stopped());
    assert!(!process.is_zombie());

    // RUNNING -> ZOMBIE
    process.transition_to_zombie();
    assert!(!process.is_running());
    assert!(!process.is_stopped());
    assert!(process.is_zombie());
}

#[test]
fn invalid_transitions() {
    let process = init_proc().new_child();

    // STOPPED -> STOPPED (idempotent)
    process.transition_to_stopped();
    assert!(process.is_stopped());
    process.transition_to_stopped();
    assert!(process.is_stopped());

    // STOPPED -> ZOMBIE
    process.transition_to_zombie();
    assert!(process.is_zombie());

    // ZOMBIE -> RUNNING (Invalid)
    process.transition_to_running();
    assert!(process.is_zombie());
    assert!(!process.is_running());

    // ZOMBIE -> STOPPED (Invalid)
    process.transition_to_stopped();
    assert!(process.is_zombie());
    assert!(!process.is_stopped());
}

#[test]
fn concurrent_transitions() {
    let process = init_proc().new_child();
    let barrier = Arc::new(Barrier::new(4));

    // Strategy:
    // Spawn 4 threads to change the state concurrently.
    // All of them start at the same time after all 4 barriers are reached,
    // simulating multiple kernel threads accessing and changing the process state
    // simultaneously.
    let mut handles = vec![];

    // Thread 1: Tries to stop
    let p1 = process.clone();
    let b1 = barrier.clone();
    handles.push(thread::spawn(move || {
        b1.wait();
        for _ in 0..1000 {
            p1.transition_to_stopped();
            thread::yield_now();
        }
    }));

    // Thread 2: Tries to continue
    let p2 = process.clone();
    let b2 = barrier.clone();
    handles.push(thread::spawn(move || {
        b2.wait();
        for _ in 0..1000 {
            p2.transition_to_running();
            thread::yield_now();
        }
    }));

    // Thread 3: Validates terminal state consistency
    // validate the terminal state property:
    // once a process is a zombie, it should always be a zombie.
    let p3 = process.clone();
    let b3 = barrier.clone();
    handles.push(thread::spawn(move || {
        b3.wait();
        let mut observed_zombie = false;
        for _ in 0..1000 {
            if p3.is_zombie() {
                observed_zombie = true;
            } else if observed_zombie {
                // If we previously saw zombie, we should never see non-zombie
                panic!("Process transitioned from zombie to non-zombie state!");
            }
            thread::yield_now();
        }
    }));

    // Thread 4: The killer
    let p4 = process.clone();
    let b4 = barrier.clone();
    handles.push(thread::spawn(move || {
        b4.wait();
        thread::sleep(Duration::from_millis(50));
        p4.transition_to_zombie();
    }));

    for h in handles {
        h.join().unwrap();
    }

    // Must be zombie at the end
    assert!(process.is_zombie());
    assert!(!process.is_running());
    assert!(!process.is_stopped());
}
