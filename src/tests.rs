use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::{ok, Future, FutureResult};
use futures::lazy;
use tokio_threadpool::{self as threadpool, ThreadPool};

use crate::{Manage, Pool};

struct TestManager {
    counter: AtomicUsize,
}

impl TestManager {
    pub fn new() -> Self {
        Self {
            counter: AtomicUsize::new(0),
        }
    }
}

impl Manage for TestManager {
    type Resource = usize;

    type Error = ();

    type Future = FutureResult<Self::Resource, Self::Error>;

    fn create(&self) -> Self::Future {
        ok(self.counter.fetch_add(1, Ordering::SeqCst))
    }
}

fn build_pool() -> ThreadPool {
    threadpool::Builder::new().build()
}

trait ExpectedTraits: Clone + Send {}

impl ExpectedTraits for Pool<TestManager> {}

#[test]
fn check_out_one_resource() {
    let runtime = build_pool();

    let (pool, librarian) = Pool::new(4, TestManager::new());
    runtime.spawn(librarian);
    let handle = runtime.spawn_handle(pool.check_out());
    assert_eq!(*handle.wait().unwrap(), 0);
}

#[test]
fn check_out_all_resources() {
    let runtime = build_pool();

    let (pool, librarian) = Pool::new(4, TestManager::new());
    runtime.spawn(librarian);
    let entry_0 = runtime.spawn_handle(pool.check_out()).wait().unwrap();
    let entry_1 = runtime.spawn_handle(pool.check_out()).wait().unwrap();
    let entry_2 = runtime.spawn_handle(pool.check_out()).wait().unwrap();
    let entry_3 = runtime.spawn_handle(pool.check_out()).wait().unwrap();
    assert_eq!(*entry_0, 0);
    assert_eq!(*entry_1, 1);
    assert_eq!(*entry_2, 2);
    assert_eq!(*entry_3, 3);
}

#[test]
fn check_out_more_resources() {
    let runtime = build_pool();

    let (pool, librarian) = Pool::new(4, TestManager::new());
    runtime.spawn(librarian);
    let entry_0 = runtime.spawn_handle(pool.check_out()).wait().unwrap();
    let _entry_1 = runtime.spawn_handle(pool.check_out()).wait().unwrap();
    let _entry_2 = runtime.spawn_handle(pool.check_out()).wait().unwrap();
    let _entry_3 = runtime.spawn_handle(pool.check_out()).wait().unwrap();

    let mut handle = runtime.spawn_handle(pool.check_out());
    let handle = runtime
        .spawn_handle(lazy(move || {
            assert!(handle.poll().unwrap().is_not_ready());
            ok::<_, ()>(handle)
        }))
        .wait()
        .unwrap();

    mem::drop(entry_0);

    let entry_0 = handle.wait().unwrap();
    assert_eq!(0, *entry_0);
}
