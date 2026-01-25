use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{any::Any, fmt};

use kspin::SpinNoIrq;
use weak_map::WeakMap;

use crate::{Pid, ProcessGroup};

/// A [`Session`] is a collection of [`ProcessGroup`]s.
pub struct Session {
    sid: Pid,
    pub(crate) process_groups: SpinNoIrq<WeakMap<Pid, Weak<ProcessGroup>>>,
    terminal: SpinNoIrq<Option<Arc<dyn Any + Send + Sync>>>,
}

impl Session {
    /// Create a new [`Session`].
    pub(crate) fn new(sid: Pid) -> Arc<Self> {
        Arc::new(Self {
            sid,
            process_groups: SpinNoIrq::new(WeakMap::new()),
            terminal: SpinNoIrq::new(None),
        })
    }
}

impl Session {
    /// The [`Session`] ID.
    pub fn sid(&self) -> Pid {
        self.sid
    }

    /// The [`ProcessGroup`]s that belong to this [`Session`].
    pub fn process_groups(&self) -> Vec<Arc<ProcessGroup>> {
        self.process_groups.lock().values().collect()
    }

    /// Sets the terminal for this session.
    pub fn set_terminal_with(&self, terminal: impl FnOnce() -> Arc<dyn Any + Send + Sync>) -> bool {
        let mut guard = self.terminal.lock();
        if guard.is_some() {
            return false;
        }
        *guard = Some(terminal());
        true
    }

    /// Unsets the terminal for this session if it is the given terminal.
    pub fn unset_terminal(&self, term: &Arc<dyn Any + Send + Sync>) -> bool {
        let mut guard = self.terminal.lock();
        if guard.as_ref().is_some_and(|it| Arc::ptr_eq(it, term)) {
            *guard = None;
            true
        } else {
            false
        }
    }

    /// Gets the terminal for this session, if it exists.
    pub fn terminal(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.terminal.lock().clone()
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session({})", self.sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Process;
    use crate::process::is_init_initialized;
    use alloc::{format, string::ToString};

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
        static COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(3000);
        COUNTER.fetch_add(1, core::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn test_sid() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        assert_eq!(session.sid(), pid);
    }

    #[test]
    fn test_process_groups() {
        ensure_init();
        let init = crate::init_proc();
        let session = init.group().session();

        let groups = session.process_groups();
        assert!(groups.iter().any(|g| g.pgid() == init.pid()));
        assert!(groups.len() >= 1);
    }

    #[test]
    fn test_process_groups_multiple() {
        ensure_init();
        let init = crate::init_proc();
        let session = init.group().session();

        let groups_before = session.process_groups().len();
        let child1 = init.fork(alloc_pid());
        let child2 = init.fork(alloc_pid());
        let group1 = child1.create_group().unwrap();
        let group2 = child2.create_group().unwrap();

        let groups = session.process_groups();
        assert!(groups.iter().any(|g| g.pgid() == group1.pgid()));
        assert!(groups.iter().any(|g| g.pgid() == group2.pgid()));
        assert_eq!(groups.len(), groups_before + 2);
    }

    #[test]
    fn test_set_terminal() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        let terminal: Arc<dyn Any + Send + Sync> = Arc::new(42u32);
        assert!(session.set_terminal_with(|| terminal.clone()));

        let retrieved = session.terminal().unwrap();
        assert!(Arc::ptr_eq(&retrieved, &terminal));
    }

    #[test]
    fn test_set_terminal_twice() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        let terminal1: Arc<dyn Any + Send + Sync> = Arc::new(42u32);
        let terminal2: Arc<dyn Any + Send + Sync> = Arc::new(43u32);

        assert!(session.set_terminal_with(|| terminal1.clone()));
        assert!(!session.set_terminal_with(|| terminal2.clone()));

        let retrieved = session.terminal().unwrap();
        assert!(Arc::ptr_eq(&retrieved, &terminal1));
    }

    #[test]
    fn test_unset_terminal() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        let terminal: Arc<dyn Any + Send + Sync> = Arc::new(42u32);
        session.set_terminal_with(|| terminal.clone());

        assert!(session.unset_terminal(&terminal));
        assert!(session.terminal().is_none());
    }

    #[test]
    fn test_unset_terminal_wrong() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        let terminal1: Arc<dyn Any + Send + Sync> = Arc::new(42u32);
        let terminal2: Arc<dyn Any + Send + Sync> = Arc::new(43u32);

        session.set_terminal_with(|| terminal1.clone());
        assert!(!session.unset_terminal(&terminal2));

        let retrieved = session.terminal().unwrap();
        assert!(Arc::ptr_eq(&retrieved, &terminal1));
    }

    #[test]
    fn test_unset_terminal_none() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        let terminal: Arc<dyn Any + Send + Sync> = Arc::new(42u32);
        assert!(!session.unset_terminal(&terminal));
    }

    #[test]
    fn test_terminal_none() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        assert!(session.terminal().is_none());
    }

    #[test]
    fn test_debug() {
        ensure_init();
        let pid = alloc_pid();
        let session = Session::new(pid);

        let debug_str = format!("{:?}", session);
        assert!(debug_str.contains("Session"));
        assert!(debug_str.contains(&pid.to_string()));
    }
}
