use std::cell::RefCell;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread_local;
use std::time::{Duration, Instant};

use futures::future::{ok, Future, FutureResult};
use futures::lazy;
use tokio_threadpool::{self as threadpool, ThreadPool};

use crate::pool::Env;
use crate::{Builder, CheckOut, CheckOutFuture, Manage, Pool, Status};

thread_local! {
    static NOW: RefCell<Arc<Mutex<Instant>>> = RefCell::new(Arc::new(Mutex::new(Instant::now())));
}

struct TestEnvironment;

impl Env for TestEnvironment {
    fn now() -> Instant {
        NOW.with(|now| {
            let arc = now.borrow();
            let lock = arc.lock().unwrap();
            *lock
        })
    }
}

struct Resource {
    id: usize,
    recycle_count: usize,
}

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
    type Resource = Resource;

    type CheckOut = CheckOut<Self>;

    type Error = ();

    type CreateFuture = FutureResult<Self::Resource, Self::Error>;

    fn create(&self) -> Self::CreateFuture {
        let resource = Resource {
            id: self.counter.fetch_add(1, Ordering::SeqCst),
            recycle_count: 0,
        };
        ok(resource)
    }

    fn status(&self, _resource: &Self::Resource) -> Status {
        Status::Valid
    }

    type RecycleFuture = FutureResult<Option<Self::Resource>, Self::Error>;

    fn recycle(&self, mut resource: Self::Resource) -> Self::RecycleFuture {
        resource.recycle_count += 1;
        ok(Some(resource))
    }
}

fn init_environment(now: Arc<Mutex<Instant>>) {
    NOW.with(|thread_now| {
        thread_now.replace(now);
    })
}

fn build_pool() -> (ThreadPool, Arc<Mutex<Instant>>) {
    let now = Arc::new(Mutex::new(Instant::now()));
    // When calling SpawnHandle::wait, the future may run on the calling thread in addition to any
    // thread in the pool.
    init_environment(Arc::clone(&now));

    let pool_now = Arc::clone(&now);
    let pool = threadpool::Builder::new()
        .after_start(move || init_environment(Arc::clone(&pool_now)))
        .build();

    (pool, now)
}

trait ExpectedTraits: Clone + Send {}

impl ExpectedTraits for Pool<TestManager> {}

fn check_out(pool: &Pool<TestManager>) -> CheckOutFuture<TestManager, TestEnvironment> {
    pool.check_out_with_environment::<TestEnvironment>()
}

#[test]
fn check_out_one_resource() {
    let (runtime, _) = build_pool();

    let builder = Builder::new();
    let pool = builder.build(4, TestManager::new());
    let handle = runtime.spawn_handle(check_out(&pool));
    assert_eq!(handle.wait().unwrap().id, 0);
}

#[test]
fn check_out_all_resources() {
    let (runtime, _) = build_pool();

    let builder = Builder::new();
    let pool = builder.build(4, TestManager::new());
    let entry_0 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
    let entry_1 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
    let entry_2 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
    let entry_3 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
    assert_eq!(entry_0.id, 0);
    assert_eq!(entry_1.id, 1);
    assert_eq!(entry_2.id, 2);
    assert_eq!(entry_3.id, 3);
}

#[test]
fn check_out_more_resources() {
    let (runtime, _) = build_pool();

    let builder = Builder::new();
    let pool = builder.build(4, TestManager::new());
    let entry_0 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
    let _entry_1 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
    let _entry_2 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
    let _entry_3 = runtime.spawn_handle(check_out(&pool)).wait().unwrap();

    let mut handle = runtime.spawn_handle(pool.check_out_with_environment::<TestEnvironment>());
    let handle = runtime
        .spawn_handle(lazy(move || {
            assert!(handle.poll().unwrap().is_not_ready());
            ok::<_, ()>(handle)
        }))
        .wait()
        .unwrap();

    mem::drop(entry_0);

    let entry_0 = handle.wait().unwrap();
    assert_eq!(0, entry_0.id);
}

#[test]
fn recycle_resource() {
    let (runtime, now) = build_pool();

    let pool = Builder::new()
        .recycle_interval(Duration::from_secs(30))
        .build(1, TestManager::new());

    for _ in 0..2 {
        let resource = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
        assert_eq!(0, resource.id);
        assert_eq!(0, resource.recycle_count);
    }

    {
        let mut lock = now.lock().unwrap();
        *lock += Duration::from_secs(60);
    }

    for _ in 0..2 {
        let resource = runtime.spawn_handle(check_out(&pool)).wait().unwrap();
        assert_eq!(0, resource.id);
        assert_eq!(1, resource.recycle_count);
    }
}
