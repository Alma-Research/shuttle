use crate::runtime::execution::Execution;
use crate::runtime::task_id::{TaskId, TaskSet};
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::rc::Rc;
use std::sync::{LockResult, TryLockResult};

/// A reader-writer lock
#[derive(Debug)]
pub struct RwLock<T> {
    inner: std::sync::RwLock<T>,
    state: Rc<RefCell<RwLockState>>,
}

#[derive(Debug)]
struct RwLockState {
    holder: RwLockHolder,
    waiting_readers: TaskSet,
    waiting_writers: TaskSet,
}

#[derive(PartialEq, Eq, Debug)]
enum RwLockHolder {
    Read(TaskSet),
    Write(TaskId),
    None,
}

impl<T> RwLock<T> {
    /// Create a new instance of an `RwLock<T>` which us unlocked.
    pub fn new(value: T) -> Self {
        let state = RwLockState {
            holder: RwLockHolder::None,
            waiting_readers: TaskSet::new(),
            waiting_writers: TaskSet::new(),
        };

        Self {
            inner: std::sync::RwLock::new(value),
            state: Rc::new(RefCell::new(state)),
        }
    }

    /// Locks this rwlock with shared read access, blocking the current thread until it can be
    /// acquired.
    pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> {
        self.lock(false);

        let inner = self.inner.try_read().expect("rwlock state out of sync");

        Ok(RwLockReadGuard {
            inner: Some(inner),
            state: Rc::clone(&self.state),
        })
    }

    /// Locks this rwlock with exclusive write access, blocking the current thread until it can
    /// be acquired.
    pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> {
        self.lock(true);

        let inner = self.inner.try_write().expect("rwlock state out of sync");

        Ok(RwLockWriteGuard {
            inner: Some(inner),
            state: Rc::clone(&self.state),
        })
    }

    /// Attempts to acquire this rwlock with shared read access.
    ///
    /// If the access could not be granted at this time, then Err is returned. This function does
    /// not block.
    pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<T>> {
        unimplemented!()
    }

    /// Attempts to acquire this rwlock with shared read access.
    ///
    /// If the access could not be granted at this time, then Err is returned. This function does
    /// not block.
    pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<T>> {
        unimplemented!()
    }

    /// Consumes this `RwLock`, returning the underlying data
    pub fn into_inner(self) -> LockResult<T> {
        assert_eq!(self.state.borrow().holder, RwLockHolder::None);
        self.inner.into_inner()
    }

    fn lock(&self, write: bool) {
        let me = Execution::me();

        let mut state = self.state.borrow_mut();
        // We are waiting for the lock
        if write {
            state.waiting_writers.insert(me);
        } else {
            state.waiting_readers.insert(me);
        }
        // Block if the lock is in a state where we can't acquire it immediately
        match &state.holder {
            RwLockHolder::Write(writer) => {
                assert_ne!(*writer, me);
                Execution::with_state(|s| s.current_mut().block());
            }
            RwLockHolder::Read(readers) => {
                assert!(!readers.contains(me));
                if write {
                    Execution::with_state(|s| s.current_mut().block());
                }
            }
            _ => {}
        }
        drop(state);

        // Acquiring a lock is a yield point
        Execution::switch();

        let mut state = self.state.borrow_mut();
        // Once the scheduler has resumed this thread, we are clear to take the lock. We might
        // not actually be in the waiters, though (if the lock was uncontended).
        // TODO should always be in the waiters?
        match (write, &mut state.holder) {
            (true, RwLockHolder::None) => {
                state.holder = RwLockHolder::Write(me);
            }
            (false, RwLockHolder::None) => {
                let mut readers = TaskSet::new();
                readers.insert(me);
                state.holder = RwLockHolder::Read(readers);
            }
            (false, RwLockHolder::Read(readers)) => {
                readers.insert(me);
            }
            _ => {
                panic!(
                    "resumed a waiting thread (writer={}) while the lock was in state {:?}",
                    write, state.holder
                );
            }
        }
        if write {
            state.waiting_writers.remove(me);
        } else {
            state.waiting_readers.remove(me);
        }
        state.waiting_readers.remove(me);
        // Block all other waiters, since we won the race to take this lock
        // TODO a bit of a bummer that we have to do this (it would be cleaner if those threads
        // TODO never become unblocked), but might need to track more state to avoid this.
        Self::block_waiters(&*state, me);
        drop(state);
    }

    fn block_waiters(state: &RwLockState, me: TaskId) {
        for tid in state.waiting_readers.iter().chain(state.waiting_writers.iter()) {
            assert_ne!(tid, me);
            Execution::with_state(|s| s.get_mut(tid).block());
        }
    }

    fn unblock_waiters(state: &RwLockState, me: TaskId, should_be_blocked: bool) {
        for tid in state.waiting_readers.iter().chain(state.waiting_writers.iter()) {
            assert_ne!(tid, me);
            Execution::with_state(|s| {
                if should_be_blocked {
                    s.get_mut(tid).unblock();
                } else {
                    s.get_mut(tid).maybe_unblock();
                }
            });
        }
    }
}

// Safety: RwLock is never actually passed across true threads, only across continuations. The
// Rc<RefCell<_>> type therefore can't be preempted mid-bookkeeping-operation.
// TODO we shouldn't need to do this, but RefCell is not Send, and anything we put within a RwLock
// TODO needs to be Send.
unsafe impl<T> Send for RwLock<T> {}
unsafe impl<T> Sync for RwLock<T> {}

// TODO this is the RefCell biting us again
impl<T> UnwindSafe for RwLock<T> {}
impl<T> RefUnwindSafe for RwLock<T> {}

/// RAII structure used to release the shared read access of a `RwLock` when dropped.
#[derive(Debug)]
pub struct RwLockReadGuard<'a, T> {
    inner: Option<std::sync::RwLockReadGuard<'a, T>>,
    state: Rc<RefCell<RwLockState>>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().unwrap().deref()
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.inner = None;

        // Unblock every thread waiting on this lock. The scheduler will choose one of them to win
        // the race to this lock, and that thread will re-block all the losers.
        let me = Execution::me();
        let mut state = self.state.borrow_mut();
        match &mut state.holder {
            RwLockHolder::Read(readers) => {
                readers.remove(me);
                if readers.is_empty() {
                    state.holder = RwLockHolder::None;
                }
            }
            _ => panic!("exiting a reader but rwlock is in the wrong state"),
        }
        RwLock::<T>::unblock_waiters(&*state, me, false);
        drop(state);

        // Releasing a lock is a yield point
        Execution::switch();
    }
}

/// RAII structure used to release the exclusive write access of a `RwLock` when dropped.
#[derive(Debug)]
pub struct RwLockWriteGuard<'a, T> {
    inner: Option<std::sync::RwLockWriteGuard<'a, T>>,
    state: Rc<RefCell<RwLockState>>,
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.inner = None;

        // Unblock every thread waiting on this lock. The scheduler will choose one of them to win
        // the race to this lock, and that thread will re-block all the losers.
        let me = Execution::me();
        let mut state = self.state.borrow_mut();
        assert_eq!(state.holder, RwLockHolder::Write(me));
        state.holder = RwLockHolder::None;
        RwLock::<T>::unblock_waiters(&*state, me, true);
        drop(state);

        // Releasing a lock is a yield point
        Execution::switch();
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().unwrap().deref()
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().unwrap().deref_mut()
    }
}