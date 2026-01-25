use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt;

use kspin::SpinNoIrq;
use weak_map::WeakMap;

use crate::{Pid, Process, Session};

/// A [`ProcessGroup`] is a collection of [`Process`]es.
pub struct ProcessGroup {
    pgid: Pid,
    pub(crate) session: Arc<Session>,
    pub(crate) processes: SpinNoIrq<WeakMap<Pid, Weak<Process>>>,
}

impl ProcessGroup {
    /// Create a new [`ProcessGroup`] within a [`Session`].
    pub(crate) fn new(pgid: Pid, session: &Arc<Session>) -> Arc<Self> {
        let group = Arc::new(Self {
            pgid,
            session: session.clone(),
            processes: SpinNoIrq::new(WeakMap::new()),
        });
        session.process_groups.lock().insert(pgid, &group);
        group
    }
}

impl ProcessGroup {
    /// The [`ProcessGroup`] ID.
    pub fn pgid(&self) -> Pid {
        self.pgid
    }

    /// The [`Session`] that the [`ProcessGroup`] belongs to.
    pub fn session(&self) -> Arc<Session> {
        self.session.clone()
    }

    /// The [`Process`]es that belong to this [`ProcessGroup`].
    pub fn processes(&self) -> Vec<Arc<Process>> {
        self.processes.lock().values().collect()
    }
}

impl fmt::Debug for ProcessGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ProcessGroup({}, session={})",
            self.pgid,
            self.session.sid()
        )
    }
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::ToString};

    use super::*;
    use crate::{Process, process::is_init_initialized};

    fn ensure_init() {
        // Try to get init proc, if it fails, initialize it
        if !is_init_initialized() {
            // Use a static flag to prevent multiple initializations
            static INIT_FLAG: core::sync::atomic::AtomicBool =
                core::sync::atomic::AtomicBool::new(false);
            if !INIT_FLAG.swap(true, core::sync::atomic::Ordering::SeqCst) {
                Process::new_init(alloc_pid());
            } else {
                // Another thread/test already initialized, wait a bit and check again
                // In single-threaded tests, this shouldn't happen, but be safe
                while !is_init_initialized() {
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn alloc_pid() -> Pid {
        static COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(2000);
        COUNTER.fetch_add(1, core::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn test_pgid() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);
        let group = ProcessGroup::new(pid, &session);

        assert_eq!(group.pgid(), pid);
    }

    #[test]
    fn test_session() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);
        let group = ProcessGroup::new(pid, &session);

        assert_eq!(group.session().sid(), session.sid());
    }

    #[test]
    fn test_processes() {
        ensure_init();
        let init = crate::init_proc();
        let group = init.group();

        let processes_before = group.processes().len();
        let child = init.fork(alloc_pid());

        let processes = group.processes();
        assert!(processes.iter().any(|p| p.pid() == init.pid()));
        assert!(processes.iter().any(|p| p.pid() == child.pid()));
        assert_eq!(processes.len(), processes_before + 1);
    }

    #[test]
    fn test_processes_empty_group() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);
        let group = ProcessGroup::new(alloc_pid(), &session);

        let processes = group.processes();
        assert!(processes.is_empty());
    }

    #[test]
    fn test_debug() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);
        let group = ProcessGroup::new(pid, &session);

        let debug_str = format!("{:?}", group);
        assert!(debug_str.contains("ProcessGroup"));
        assert!(debug_str.contains(&pid.to_string()));
    }
}
