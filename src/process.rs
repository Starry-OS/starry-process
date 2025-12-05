use alloc::{
    collections::btree_set::BTreeSet,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicU8, Ordering},
};

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

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(crate) struct ProcessState: u8 {
        const RUNNING = 1 << 0;
        const STOPPED = 1 << 1;
        const ZOMBIE  = 1 << 2;
    }
}

impl PartialEq<u8> for ProcessState {
    fn eq(&self, other: &u8) -> bool {
        self.bits() == *other
    }
}

impl PartialEq<ProcessState> for u8 {
    fn eq(&self, other: &ProcessState) -> bool {
        *self == other.bits()
    }
}

/// A process.
pub struct Process {
    pid: Pid,
    state: AtomicU8,
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

/// Status & exit
impl Process {
    /// Returns `true` if the [`Process`] is a zombie process.
    pub fn is_zombie(&self) -> bool {
        self.state.load(Ordering::Acquire) == ProcessState::ZOMBIE
    }

    /// Returns `true` if the [`Process`] is running.
    pub fn is_running(&self) -> bool {
        self.state.load(Ordering::Acquire) == ProcessState::RUNNING
    }

    ///  Returns `true` if the [`Process`] is stopped.
    pub fn is_stopped(&self) -> bool {
        self.state.load(Ordering::Acquire) == ProcessState::STOPPED
    }

    /// Change the [`Process`] from Running to `Stopped`.
    ///
    /// This method atomically transitions the process state to STOPPED using
    /// CAS, ensuring the state is either successfully changed or already in
    /// ZOMBIE state (in which case no change occurs).
    ///
    /// # Memory Ordering
    ///
    /// Uses `Release` ordering on success to synchronize with `Acquire` loads
    /// in `is_stopped()`. This ensures that any writes before this
    /// transition, such as setting `stop_signal` in the
    /// `ProcessSignalManager`, are visible to threads that observe the
    /// `STOPPED` state transition.
    pub fn transition_to_stopped(&self) {
        let _ = self.state.fetch_update(
            Ordering::Release, // Success: synchronize with is_stopped()
            Ordering::Relaxed, // Failure: no synchronization needed
            |curr| {
                if curr == ProcessState::ZOMBIE.bits() {
                    None // Already zombie, don't transition
                } else {
                    Some(ProcessState::STOPPED.bits())
                }
            },
        );
    }

    /// Change the [`Process`] from `Stopped` to `Running`.
    ///
    /// This method atomically transitions the process state to RUNNING using
    /// CAS. The transition succeeds if and only if the current state is
    /// `STOPPED`.
    ///
    /// # Memory Ordering
    ///
    /// Uses `Release` ordering on success to synchronize with `Acquire` loads
    /// in `is_running()`. This ensures that any writes before this
    /// transition, for example, setting `cont_signal` in the
    /// `ProcessSignalManager`, are visible to threads that observe the
    /// `RUNNING` state transition.
    pub fn transition_to_running(&self) {
        let _ = self.state.fetch_update(
            Ordering::Release, // Success: synchronize with is_running()
            Ordering::Relaxed, // Failure: no synchronization needed
            |curr| {
                if curr != ProcessState::STOPPED.bits() {
                    None // Not stopped, don't transition
                } else {
                    Some(ProcessState::RUNNING.bits())
                }
            },
        );
    }

    /// Change the [`Process`] from `Stopped` or `Running` to `Zombie`.
    ///
    /// This is a terminal state transition - once a process becomes a zombie,
    /// it cannot transition to any other state (it can only be freed via
    /// `free()`).
    ///
    /// # Memory Ordering
    ///
    /// Uses `Release` ordering to synchronize with `Acquire` loads in
    /// `is_zombie()`, ensuring that when a parent process's `wait()` observes
    /// the ZOMBIE state, it also observes all writes that happened before
    /// the transition, particularly the exit_code set by `exit_thread()`.
    pub fn transition_to_zombie(&self) {
        if self.is_zombie() {
            return;
        }

        self.state.store(
            ProcessState::ZOMBIE.bits(),
            Ordering::Release, // Synchronize with is_zombie()
        );
    }

    /// Terminates the [`Process`].
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

        let mut children = self.children.lock(); // Acquire the lock first

        let mut reaper_children = reaper.children.lock();
        let reaper = Arc::downgrade(reaper);

        for (pid, child) in core::mem::take(&mut *children) {
            *child.parent.lock() = reaper.clone();
            reaper_children.insert(pid, child);
        }
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
            state: AtomicU8::new(ProcessState::RUNNING.bits()),
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
    pub fn fork(self: &Arc<Process>, pid: Pid) -> Arc<Process> {
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
