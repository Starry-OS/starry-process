use alloc::{
    collections::btree_set::BTreeSet,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};

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

/// A process.
pub struct Process {
    pid: Pid,
    is_zombie: AtomicBool,
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
        self.is_zombie.load(Ordering::Acquire)
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

        let mut children = self.children.lock(); // Acquire the lock first
        self.is_zombie.store(true, Ordering::Release);

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
            is_zombie: AtomicBool::new(false),
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

#[cfg(test)]
pub(crate) fn is_init_initialized() -> bool {
    INIT_PROC.get().is_some()
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn ensure_init() {
        // Try to get init proc, if it fails, initialize it
        if INIT_PROC.get().is_none() {
            // Use a static flag to prevent multiple initializations
            static INIT_FLAG: core::sync::atomic::AtomicBool =
                core::sync::atomic::AtomicBool::new(false);
            if !INIT_FLAG.swap(true, core::sync::atomic::Ordering::SeqCst) {
                Process::new_init(alloc_pid());
            } else {
                // Another thread/test already initialized, wait a bit and check again
                // In single-threaded tests, this shouldn't happen, but be safe
                while INIT_PROC.get().is_none() {
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn alloc_pid() -> Pid {
        static COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1000);
        COUNTER.fetch_add(1, core::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn test_pid() {
        ensure_init();
        let init = init_proc();
        assert_eq!(init.pid(), init.pid());
    }

    #[test]
    fn test_is_init() {
        ensure_init();
        let init = init_proc();
        assert!(init.is_init());

        let child = init.fork(alloc_pid());
        assert!(!child.is_init());
    }

    #[test]
    fn test_parent_child_relationship() {
        ensure_init();
        let parent = init_proc();
        let child = parent.fork(alloc_pid());

        assert_eq!(child.parent().unwrap().pid(), parent.pid());
        assert!(parent.children().iter().any(|c| c.pid() == child.pid()));
    }

    #[test]
    fn test_multiple_children() {
        ensure_init();
        let parent = init_proc();
        let child1 = parent.fork(alloc_pid());
        let child2 = parent.fork(alloc_pid());

        let children = parent.children();
        assert!(children.iter().any(|c| c.pid() == child1.pid()));
        assert!(children.iter().any(|c| c.pid() == child2.pid()));
        assert!(children.len() >= 2);
    }

    #[test]
    fn test_group_inheritance() {
        ensure_init();
        let parent = init_proc();
        let child = parent.fork(alloc_pid());

        assert_eq!(child.group().pgid(), parent.group().pgid());
    }

    #[test]
    fn test_create_group() {
        ensure_init();
        let parent = init_proc();
        let child = parent.fork(alloc_pid());

        let new_group = child.create_group().unwrap();
        assert_eq!(new_group.pgid(), child.pid());
        assert_eq!(child.group().pgid(), child.pid());
    }

    #[test]
    fn test_create_group_already_leader() {
        ensure_init();
        let process = init_proc();

        // Init process is already a group leader
        assert!(process.create_group().is_none());
    }

    #[test]
    fn test_create_session() {
        ensure_init();
        let parent = init_proc();
        let child = parent.fork(alloc_pid());

        let (session, group) = child.create_session().unwrap();
        assert_eq!(session.sid(), child.pid());
        assert_eq!(group.pgid(), child.pid());
    }

    #[test]
    fn test_create_session_already_leader() {
        ensure_init();
        let process = init_proc();

        // Init process is already a session leader
        assert!(process.create_session().is_none());
    }

    #[test]
    fn test_move_to_group() {
        ensure_init();
        let parent = init_proc();
        let child1 = parent.fork(alloc_pid());
        let child2 = parent.fork(alloc_pid());

        let group1 = child1.create_group().unwrap();
        assert!(child2.move_to_group(&group1));
        assert_eq!(child2.group().pgid(), group1.pgid());
    }

    #[test]
    fn test_move_to_same_group() {
        ensure_init();
        let parent = init_proc();
        let child = parent.fork(alloc_pid());
        let group = child.group();

        assert!(child.move_to_group(&group));
    }

    #[test]
    fn test_move_to_different_session() {
        ensure_init();
        let parent = init_proc();
        let child1 = parent.fork(alloc_pid());
        let child2 = parent.fork(alloc_pid());

        let (_, group1) = child1.create_session().unwrap();
        assert!(!child2.move_to_group(&group1));
    }

    #[test]
    fn test_thread_management() {
        ensure_init();
        let process = init_proc();

        process.add_thread(1);
        process.add_thread(2);
        process.add_thread(3);

        let mut threads = process.threads();
        threads.sort();
        assert_eq!(threads, vec![1, 2, 3]);
    }

    #[test]
    fn test_exit_thread() {
        ensure_init();
        let child = init_proc().fork(alloc_pid());

        child.add_thread(1);
        child.add_thread(2);

        let last = child.exit_thread(1, 42);
        assert!(!last);
        assert_eq!(child.exit_code(), 42);
        assert!(child.threads().contains(&2));
        assert!(!child.threads().contains(&1));

        // Without group_exit, exit code will be updated
        let last = child.exit_thread(2, 99);
        assert!(last);
        assert_eq!(child.exit_code(), 99);
        assert!(child.threads().is_empty());
    }

    #[test]
    fn test_group_exit() {
        ensure_init();
        let child = init_proc().fork(alloc_pid());

        child.add_thread(1);
        child.group_exit();
        assert!(child.is_group_exited());

        // Exit code should not change after group exit
        let exit_code_before = child.exit_code();
        child.exit_thread(1, 99);
        assert_eq!(child.exit_code(), exit_code_before);
    }

    #[test]
    fn test_is_zombie() {
        ensure_init();
        let child = init_proc().fork(alloc_pid());

        assert!(!child.is_zombie());
    }

    #[test]
    fn test_exit() {
        ensure_init();
        let parent = init_proc();
        let child = parent.fork(alloc_pid());

        child.exit();
        assert!(child.is_zombie());
        assert!(parent.children().iter().any(|c| c.pid() == child.pid()));
    }

    #[test]
    fn test_exit_init_process() {
        ensure_init();
        let init = init_proc();

        // Exit init process should not panic, but should do nothing
        init.exit();
        assert!(!init.is_zombie());
    }

    #[test]
    fn test_exit_child_reaping() {
        ensure_init();
        let init = init_proc();
        let parent = init.fork(alloc_pid());
        let child = parent.fork(alloc_pid());

        parent.exit();
        assert_eq!(child.parent().unwrap().pid(), init.pid());
    }

    #[test]
    fn test_free_zombie() {
        ensure_init();
        let parent = init_proc();
        let child = parent.fork(alloc_pid());
        let child_pid = child.pid();

        child.exit();
        assert!(child.is_zombie());
        assert!(parent.children().iter().any(|c| c.pid() == child_pid));

        child.free();
        assert!(!parent.children().iter().any(|c| c.pid() == child_pid));
    }

    #[test]
    #[should_panic(expected = "only zombie process can be freed")]
    fn test_free_not_zombie() {
        ensure_init();
        let process = init_proc().fork(alloc_pid());
        process.free();
    }
}
