// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. https://www.kylinos.cn/
// Copyright (C) 2025 Azure-stars Azure_stars@126.com
// See LICENSES for license details.

//! Process state management implementation.
//!
//! # Architecture Overview
//!
//! This module implements a composite process state machine that tracks both
//! the primary lifecycle state of a process and auxiliary event flags for
//! parent notification via `waitpid`.
//!
//! ## State Model
//!
//! The process state is represented by two components:
//!
//! 1. **`ProcessStateKind`** - The primary lifecycle state:
//!    - `Running`: Process is executing or ready to execute.
//!    - `Stopped`: Process is stopped by a signal or ptrace.
//!    - `Zombie`: Process has terminated but not yet been reaped.
//!
//! 2. **`ProcessStateFlags`** - Event acknowledgement flags:
//!    - `STOPPED_UNACKED`: A stop event has occurred but not yet reported to
//!      parent.
//!    - `CONTINUED_UNACKED`: A continuation event has occurred but not yet
//!      reported to parent.
//!
//! These are combined in the `ProcessState` struct, which encapsulates all
//! state transitions and ensures invariants are maintained.
//!
//! ## State Transitions
//!
//! State transitions are managed through explicit methods on `ProcessState`:
//!
//! - `transition_to_stopped(signal, ptraced)`: `Running` → `Stopped` +
//!   `STOPPED_UNACKED`
//! - `transition_to_running()`: `Stopped` → `Running` + `CONTINUED_UNACKED`
//! - `transition_to_zombie(info)`: Any state → `Zombie` (clears all flags)
//!
//! ### Important Invariants
//!
//! - A `Zombie` process cannot transition to any other state.
//! - The `CONTINUED_UNACKED` flag is only valid when `kind` is `Running`.
//! - The `STOPPED_UNACKED` flag is only valid when `kind` is `Stopped`.
//! - Flags are automatically set during transitions and consumed atomically by
//!   `waitpid`.
//!
//! ## Interaction with `waitpid`
//!
//! Parent processes use `waitpid` with options like `WUNTRACED` and
//! `WCONTINUED` to wait for child state changes. The event consumption flow is:
//!
//! 1. Child transitions (e.g., `Running` → `Stopped`), setting the
//!    corresponding flag.
//! 2. Parent calls `waitpid(WUNTRACED)`, which internally calls
//!    `try_consume_stopped()`.
//! 3. `try_consume_stopped()` atomically checks and clears the
//!    `STOPPED_UNACKED` flag.
//! 4. If successful, the event is reported exactly once to the parent.
//!
//! This ensures that each state change event is reported exactly once,
//! preventing duplicate notifications and race conditions.
//!
//! ## Thread Safety
//!
//! The `ProcessState` is protected by a `SpinNoIrq` lock within the `Process`
//! struct. All state queries and transitions must acquire this lock, ensuring
//! atomic updates even in concurrent scenarios (e.g., signal delivery while
//! parent is waiting).
//!
//! ## Ptrace Integration
//!
//! Ptrace stops are represented as `Stopped { ptraced: true, signal }`. They
//! differ from signal-stops in key ways:
//!
//! - Ptrace stops are NOT consumed by standard `waitpid(WUNTRACED)` calls.
//! - They are handled separately via `check_ptrace_stop()` in the ptrace
//!   subsystem.
//! - Resuming from ptrace stops goes directly to `Running` (no `CONTINUED`
//!   event).

use alloc::{
    collections::btree_set::BTreeSet,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt;

use bitflags::bitflags;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use weak_map::StrongMap;

use crate::{Pid, ProcessGroup, Session};

#[derive(Default)]
pub(crate) struct ThreadGroup {
    pub(crate) threads: BTreeSet<Pid>,
    pub(crate) exit_code: i32,
    pub(crate) group_exited: bool,
}

/// The primary lifecycle state of the process.
///
/// We create three states for process, `Running`, `Stopped`, and `Zombie`.
///
/// For a `Running` process, it can be actually running(if the
/// `ProcessStateFlags` is empty) or it can be just `Continued` from a stoppage
/// but not acked by its parent(if the `ProcessStateFlags` contain
/// `CONTINUED_UNACKED`).
///
/// For a `Stopped` process, if its stoppage has not been acked by its parent,
/// i.e., the parent has not been notified for the child's stoppade, the
/// corresponding `ProcessStateFlags` will be marked as `STOPPED_UNACKED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStateKind {
    Running,
    Stopped { signal: i32, ptraced: bool },
    Zombie { info: ZombieInfo },
}

bitflags! {
    /// Composite flags for process state (e.g., reporting status).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ProcessStateFlags: u8 {
        // A status of a process who has stopped but its stoppage
        // has not been acked by its parent
        const STOPPED_UNACKED = 1 << 0;
        // A status of a process who has just continued but its continuation
        // has not been acked by its parent
        const CONTINUED_UNACKED = 1 << 1;
    }
}

/// The exit code value following POSIX conventions.
///
/// This type encapsulates the numeric exit code that is reported to the parent
/// process via `waitpid`. The encoding follows POSIX standards:
/// - Normal exit: Exit code 0-255 directly
/// - Signal termination: Exit code = 128 + signal number
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(i32);

impl ExitCode {
    /// Creates an exit code from a normal exit.
    pub fn from_code(code: i32) -> Self {
        Self(code)
    }

    /// Creates an exit code from signal termination (128 + signal).
    pub fn from_signal(signal: i32) -> Self {
        Self(128 + signal)
    }

    /// Returns the raw exit code value.
    pub fn as_raw(self) -> i32 {
        self.0
    }
}

/// Information about a zombie (terminated) process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZombieInfo {
    /// The exit code value.
    pub exit_code: ExitCode,
    /// The signal that terminated the process, if any.
    pub signal: Option<i32>,
    /// Whether a core dump was produced.
    pub core_dumped: bool,
}

/// The full process state machine.
#[derive(Debug, Clone, Copy)]
pub struct ProcessState {
    kind: ProcessStateKind,
    flags: ProcessStateFlags,
}

/// A process.
pub struct Process {
    pid: Pid,
    state: SpinNoIrq<ProcessState>,
    pub(crate) tg: SpinNoIrq<ThreadGroup>,

    // TODO: child subreaper9
    children: SpinNoIrq<StrongMap<Pid, Arc<Process>>>,
    parent: SpinNoIrq<Weak<Process>>,

    group: SpinNoIrq<Arc<ProcessGroup>>,
}

impl Process {
    /// The [`Process`] ID.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Returns `true` if the [`Process`] is the init process.
    ///
    /// This is a convenience method for checking if the [`Process`]
    /// [`Arc::ptr_eq`]s with the init process, which is cheaper than
    /// calling [`init_proc`] or testing if [`Process::parent`] is `None`.
    pub fn is_init(self: &Arc<Self>) -> bool {
        Arc::ptr_eq(self, INIT_PROC.get().unwrap())
    }
}

/// Parent & children
impl Process {
    /// The parent [`Process`].
    pub fn parent(&self) -> Option<Arc<Process>> {
        self.parent.lock().upgrade()
    }

    /// The child [`Process`]es.
    pub fn children(&self) -> Vec<Arc<Process>> {
        self.children.lock().values().cloned().collect()
    }
}

/// [`ProcessGroup`] & [`Session`]
impl Process {
    /// The [`ProcessGroup`] that the [`Process`] belongs to.
    pub fn group(&self) -> Arc<ProcessGroup> {
        self.group.lock().clone()
    }

    fn set_group(self: &Arc<Self>, group: &Arc<ProcessGroup>) {
        let mut self_group = self.group.lock();

        self_group.processes.lock().remove(&self.pid);

        group.processes.lock().insert(self.pid, self);

        *self_group = group.clone();
    }

    /// Creates a new [`Session`] and new [`ProcessGroup`] and moves the
    /// [`Process`] to it.
    ///
    /// If the [`Process`] is already a session leader, this method does
    /// nothing and returns `None`.
    ///
    /// Otherwise, it returns the new [`Session`] and [`ProcessGroup`].
    ///
    /// The caller has to ensure that the new [`ProcessGroup`] does not conflict
    /// with any existing [`ProcessGroup`]. Thus, the [`Process`] must not
    /// be a [`ProcessGroup`] leader.
    ///
    /// Checking [`Session`] conflicts is unnecessary.
    pub fn create_session(self: &Arc<Self>) -> Option<(Arc<Session>, Arc<ProcessGroup>)> {
        if self.group.lock().session.sid() == self.pid {
            return None;
        }

        let new_session = Session::new(self.pid);
        let new_group = ProcessGroup::new(self.pid, &new_session);
        self.set_group(&new_group);

        Some((new_session, new_group))
    }

    /// Creates a new [`ProcessGroup`] and moves the [`Process`] to it.
    ///
    /// If the [`Process`] is already a group leader, this method does nothing
    /// and returns `None`.
    ///
    /// Otherwise, it returns the new [`ProcessGroup`].
    ///
    /// The caller has to ensure that the new [`ProcessGroup`] does not conflict
    /// with any existing [`ProcessGroup`].
    pub fn create_group(self: &Arc<Self>) -> Option<Arc<ProcessGroup>> {
        if self.group.lock().pgid() == self.pid {
            return None;
        }

        let new_group = ProcessGroup::new(self.pid, &self.group.lock().session);
        self.set_group(&new_group);

        Some(new_group)
    }

    /// Moves the [`Process`] to a specified [`ProcessGroup`].
    ///
    /// Returns `true` if the move succeeded. The move failed if the
    /// [`ProcessGroup`] is not in the same [`Session`] as the [`Process`].
    ///
    /// If the [`Process`] is already in the specified [`ProcessGroup`], this
    /// method does nothing and returns `true`.
    pub fn move_to_group(self: &Arc<Self>, group: &Arc<ProcessGroup>) -> bool {
        if Arc::ptr_eq(&self.group.lock(), group) {
            return true;
        }

        if !Arc::ptr_eq(&self.group.lock().session, &group.session) {
            return false;
        }

        self.set_group(group);
        true
    }
}

/// Threads
impl Process {
    /// Adds a thread to this [`Process`] with the given thread ID.
    pub fn add_thread(self: &Arc<Self>, tid: Pid) {
        self.tg.lock().threads.insert(tid);
    }

    /// Removes a thread from this [`Process`] and sets the exit code if the
    /// group has not exited.
    ///
    /// Returns `true` if this was the last thread in the process.
    pub fn exit_thread(self: &Arc<Self>, tid: Pid, exit_code: i32) -> bool {
        let mut tg = self.tg.lock();
        if !tg.group_exited {
            tg.exit_code = exit_code;
        }
        tg.threads.remove(&tid);
        tg.threads.is_empty()
    }

    /// Get all threads in this [`Process`].
    pub fn threads(&self) -> Vec<Pid> {
        self.tg.lock().threads.iter().cloned().collect()
    }

    /// Returns `true` if the [`Process`] is group exited.
    pub fn is_group_exited(&self) -> bool {
        self.tg.lock().group_exited
    }

    /// Marks the [`Process`] as group exited.
    pub fn group_exit(&self) {
        self.tg.lock().group_exited = true;
    }

    /// The exit code of the [`Process`].
    pub fn exit_code(&self) -> i32 {
        self.tg.lock().exit_code
    }
}

/// Status, exit, stop & cont
impl Process {
    /// Returns `true` if the [`Process`] is a zombie process.
    pub fn is_zombie(&self) -> bool {
        self.state.lock().is_zombie()
    }

    /// Get the information of the process ig it is a zombie
    pub fn get_zombie_info(&self) -> Option<ZombieInfo> {
        if let ProcessStateKind::Zombie { info } = self.state.lock().kind {
            Some(info)
        } else {
            None
        }
    }

    /// Check whether the process has stopped,
    /// including both the case which the process just stopped without acked by
    /// its parent and already acked by its parent
    pub fn is_stopped(&self) -> bool {
        let state = self.state.lock();
        matches!(state.kind, ProcessStateKind::Stopped { .. })
            || state.flags.contains(ProcessStateFlags::STOPPED_UNACKED)
    }

    /// Check whether the process has continued from the stoppage,
    /// including both the case which the process just continued without acked
    /// by its parent and already acked by its parent
    pub fn is_continued(&self) -> bool {
        let state = self.state.lock();
        matches!(state.kind, ProcessStateKind::Running)
            && state.flags.contains(ProcessStateFlags::CONTINUED_UNACKED)
    }

    /// Updating the status of a process continued from stoppage
    pub fn continue_from_stop(&self) {
        self.state.lock().transition_to_running(true);
    }

    /// Attempts to consume the 'continued' event for `waitpid(WCONTINUED)`.
    ///
    /// This is a thread-safe wrapper around
    /// `ProcessState::try_consume_continued`. It acquires the state lock
    /// and checks if the process has a pending continuation event.
    ///
    /// Returns `true` if the event was successfully consumed.
    pub fn try_consume_continued(&self) -> bool {
        self.state.lock().try_consume_continued()
    }

    /// Attempts to consume the 'stopped' event for `waitpid(WUNTRACED)`.
    ///
    /// This is a thread-safe wrapper around
    /// `ProcessState::try_consume_stopped`. It acquires the state lock and
    /// checks if the process has a pending stop event.
    ///
    /// IMPORTANT: This method explicitly ignores ptrace-stops. Ptrace stops are
    /// handled separately via `check_ptrace_stop` and are not consumed by
    /// standard `waitpid(WUNTRACED)` calls.
    ///
    /// Returns `Some(signal)` if the event was successfully consumed.
    pub fn try_consume_stopped(&self) -> Option<i32> {
        // Only consume if it's NOT a ptrace stop (for WUNTRACED)
        let mut state = self.state.lock();
        if matches!(state.kind, ProcessStateKind::Stopped { ptraced: true, .. }) {
            return None;
        }
        state.try_consume_stopped()
    }

    /// Transfers all children of this process to the init process (reaper).
    ///
    /// This is called when a process exits to ensure orphaned children are
    /// reparented to init.
    fn reaper_children(children: &mut StrongMap<Pid, Arc<Process>>) {
        let reaper = INIT_PROC.get().unwrap();
        let mut reaper_children = reaper.children.lock();
        let reaper_weak = Arc::downgrade(reaper);

        for (pid, child) in core::mem::take(children) {
            *child.parent.lock() = reaper_weak.clone();
            reaper_children.insert(pid, child);
        }
    }

    /// Terminates the [`Process`], marking it as a zombie process.
    ///
    /// Child processes are inherited by the init process or by the nearest
    /// subreaper process.
    ///
    /// This method panics if the [`Process`] is the init process.
    pub fn exit(self: &Arc<Self>) {
        // TODO: child subreaper
        let reaper = INIT_PROC.get().unwrap();

        if Arc::ptr_eq(self, reaper) {
            return;
        }

        let mut children = self.children.lock();
        let code = self.tg.lock().exit_code;
        self.state.lock().transition_to_zombie(ZombieInfo {
            exit_code: ExitCode::from_code(code),
            signal: None,
            core_dumped: false,
        });

        Self::reaper_children(&mut children);
    }

    /// Terminates the [`Process`], marking it as a zombie process ONLY when the
    /// termination is due to a signal
    ///
    /// Child processes are inherited by the init process or by the nearest
    /// subreaper process.
    ///
    /// This method panics if the [`Process`] is the init process.
    pub fn exit_with_signal(self: &Arc<Self>, signal: i32, core_dumped: bool) {
        let reaper = INIT_PROC.get().unwrap();

        if Arc::ptr_eq(self, reaper) {
            return;
        }

        let mut children = self.children.lock();
        self.state.lock().transition_to_zombie(ZombieInfo {
            exit_code: ExitCode::from_signal(signal),
            signal: Some(signal),
            core_dumped,
        });

        Self::reaper_children(&mut children);
    }

    /// Frees a zombie [`Process`]. Removes it from the parent.
    ///
    /// This method panics if the [`Process`] is not a zombie.
    pub fn free(&self) {
        assert!(self.is_zombie(), "only zombie process can be freed");

        if let Some(parent) = self.parent() {
            parent.children.lock().remove(&self.pid);
        }
    }

    /// Stops the [`Process`], marking it as stopped when a signal stops
    /// it(majorly SIGSTOP)
    pub fn stop_by_signal(&self, stop_signal: i32) {
        self.state.lock().transition_to_stopped(stop_signal, false);
    }

    /// Set the process to be stopped due to a ptrace event.
    ///
    /// This is similar to signal-stops but is only visible to the tracer,
    /// not the parent. The signal parameter is typically SIGTRAP (5) for
    /// syscall-stops and exec-stops, or the actual signal number for
    /// signal-delivery-stops.
    ///
    /// # Arguments
    /// * `signal` - Signal to report via waitpid (SIGTRAP or actual signal)
    pub fn set_ptrace_stopped(&self, signal: i32) {
        self.state.lock().transition_to_stopped(signal, true);
    }

    /// Check if the process is in a ptrace-stop state.
    ///
    /// # Returns
    /// * `true` if process is stopped due to ptrace
    /// * `false` if running, signal-stopped, continued, or zombie
    pub fn is_ptrace_stopped(&self) -> bool {
        let state = self.state.lock();
        matches!(state.kind, ProcessStateKind::Stopped { ptraced: true, .. })
    }

    /// Check if the process is in a signal-stop state (not ptrace).
    ///
    /// # Returns
    /// * `true` if process is stopped due to signal (SIGSTOP, etc.)
    /// * `false` if running, ptrace-stopped, continued, or zombie
    pub fn is_signal_stopped(&self) -> bool {
        let state = self.state.lock();
        matches!(state.kind, ProcessStateKind::Stopped { ptraced: false, .. })
    }

    /// Resume from ptrace-stop by transitioning back to Running state.
    ///
    /// This is called by the tracer via PTRACE_CONT/SYSCALL/DETACH.
    /// Unlike signal-stops which go through Continued state, ptrace
    /// resumes directly to Running.
    pub fn resume_from_ptrace_stop(&self) {
        self.state.lock().transition_to_running(false);
    }
}

impl fmt::Debug for Process {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("Process");
        builder.field("pid", &self.pid);

        let tg = self.tg.lock();
        if tg.group_exited {
            builder.field("group_exited", &tg.group_exited);
        }
        if self.is_zombie() {
            builder.field("exit_code", &tg.exit_code);
        }

        if let Some(parent) = self.parent() {
            builder.field("parent", &parent.pid());
        }
        builder.field("group", &self.group());
        builder.finish()
    }
}

/// Builder
impl Process {
    fn new(pid: Pid, parent: Option<Arc<Process>>) -> Arc<Process> {
        let group = parent.as_ref().map_or_else(
            || {
                let session = Session::new(pid);
                ProcessGroup::new(pid, &session)
            },
            |p| p.group(),
        );

        let process = Arc::new(Process {
            pid,
            state: SpinNoIrq::new(ProcessState::new_running()),
            tg: SpinNoIrq::new(ThreadGroup::default()),
            children: SpinNoIrq::new(StrongMap::new()),
            parent: SpinNoIrq::new(parent.as_ref().map(Arc::downgrade).unwrap_or_default()),
            group: SpinNoIrq::new(group.clone()),
        });

        group.processes.lock().insert(pid, &process);

        if let Some(parent) = parent {
            parent.children.lock().insert(pid, process.clone());
        } else {
            INIT_PROC.init_once(process.clone());
        }

        process
    }

    /// Creates a init [`Process`].
    ///
    /// This function can be called multiple times, but
    /// [`ProcessBuilder::build`] on the the result must be called only once.
    pub fn new_init(pid: Pid) -> Arc<Process> {
        Self::new(pid, None)
    }

    /// Creates a child [`Process`].
    pub fn fork(self: &Arc<Self>, pid: Pid) -> Arc<Process> {
        Self::new(pid, Some(self.clone()))
    }
}

static INIT_PROC: LazyInit<Arc<Process>> = LazyInit::new();

/// Gets the init process.
///
/// This function panics if the init process has not been initialized yet.
pub fn init_proc() -> Arc<Process> {
    INIT_PROC.get().unwrap().clone()
}

impl ProcessState {
    /// Creates a new `ProcessState` in the Running state,
    /// with its `kind` to be `ProcessStateKind::Running`,
    /// and its `flags` to be empty.
    pub fn new_running() -> Self {
        Self {
            kind: ProcessStateKind::Running,
            flags: ProcessStateFlags::empty(),
        }
    }

    /// Returns `true` if the state is Zombie.
    pub fn is_zombie(&self) -> bool {
        matches!(self.kind, ProcessStateKind::Zombie { .. })
    }

    /// Transitions the state to Stopped.
    ///
    /// This method updates the process state kind to `Stopped` with the
    /// given signal and ptrace status. It also sets the `STOPPED_UNACKED`
    /// flag, indicating that the parent has not yet acknowledged this stop
    /// event (via `waitpid` with `WUNTRACED`).
    ///
    /// If the process is already a `Zombie`, this transition is ignored.
    ///
    /// # Arguments
    /// * `signal` - The signal that caused the stop.
    /// * `ptraced` - Whether the stop is due to ptrace.s
    pub fn transition_to_stopped(&mut self, signal: i32, ptraced: bool) {
        if self.is_zombie() {
            return;
        }

        self.kind = ProcessStateKind::Stopped { signal, ptraced };
        self.flags.insert(ProcessStateFlags::STOPPED_UNACKED);
    }

    /// Transitions the process state from `Stopped` to `Running`.
    ///
    /// This method is called when a stopped process is resumed (e.g., via
    /// `SIGCONT` or `PTRACE_CONT`). It updates the state kind to `Running`.
    ///
    /// If `with_continued_flag` is true, it sets the `CONTINUED_UNACKED` flag,
    /// indicating that the parent has not yet acknowledged this continuation
    /// (via `waitpid` with `WCONTINUED`). Ptrace resumes typically pass
    /// `false`.
    ///
    /// It also clears the `STOPPED_UNACKED` flag.
    ///
    /// # Arguments
    /// * `with_continued_flag` - Whether to set the `CONTINUED_UNACKED` flag.
    pub fn transition_to_running(&mut self, with_continued_flag: bool) {
        if let ProcessStateKind::Stopped { .. } = self.kind {
            self.kind = ProcessStateKind::Running;
            if with_continued_flag {
                self.flags.insert(ProcessStateFlags::CONTINUED_UNACKED);
            }
            self.flags.remove(ProcessStateFlags::STOPPED_UNACKED);
        }
    }

    /// Transitions the process state to `Zombie`.
    ///
    /// This method is called when the process terminates. It updates the state
    /// kind to `Zombie`, no matter what the previous state of the target
    /// process is at.
    ///
    /// All state flags (e.g., `STOPPED_UNACKED`, `CONTINUED_UNACKED`) are
    /// cleared, as they are no longer relevant for a dead process.
    pub fn transition_to_zombie(&mut self, info: ZombieInfo) {
        self.kind = ProcessStateKind::Zombie { info };
        self.flags = ProcessStateFlags::empty();
    }

    /// Attempts to consume the 'stopped' event for `waitpid(WUNTRACED)`.
    ///
    /// This method checks if the process is in the `Stopped` state and if the
    /// `STOPPED_UNACKED` flag is set. If both are true, it:
    /// 1. Clears the `STOPPED_UNACKED` flag (atomically consuming the event).
    /// 2. Returns `Some(signal)` where `signal` is the signal that caused the
    ///    stop.
    ///
    /// If the process is not stopped, or if the event has already been consumed
    /// (flag is clear), it returns `None`.
    ///
    /// This ensures that a stop event is reported exactly once to a parent
    /// calling `waitpid`.
    pub fn try_consume_stopped(&mut self) -> Option<i32> {
        match self.kind {
            ProcessStateKind::Stopped { signal, .. }
                if self.flags.contains(ProcessStateFlags::STOPPED_UNACKED) =>
            {
                self.flags.remove(ProcessStateFlags::STOPPED_UNACKED);
                Some(signal)
            }
            _ => None,
        }
    }

    /// Attempts to consume the 'continued' event for `waitpid(WCONTINUED)`.
    ///
    /// This method checks if the process is in the `Running` state (which
    /// implies it might have been continued) and if the `CONTINUED_UNACKED`
    /// flag is set. If both are true, it:
    /// 1. Clears the `CONTINUED_UNACKED` flag (atomically consuming the event).
    /// 2. Returns `true`.
    ///
    /// If the process is not running, or if the event has already been consumed
    /// (flag is clear), it returns `false`.
    ///
    /// This ensures that a continuation event is reported exactly once to a
    /// parent calling `waitpid`.
    pub fn try_consume_continued(&mut self) -> bool {
        if self.flags.contains(ProcessStateFlags::CONTINUED_UNACKED) {
            self.flags.remove(ProcessStateFlags::CONTINUED_UNACKED);
            return true;
        }
        false
    }
}
