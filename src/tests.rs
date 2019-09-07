use std::cell::RefCell;
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread_local;
use std::time::{Duration, Instant};

use futures::future::{ok, Ready};

use crate::pool::Dependencies;
use crate::{Builder, CheckOut, Manage, Pool, Status};

thread_local! {
    static NOW: RefCell<Arc<Mutex<Instant>>> = RefCell::new(Arc::new(Mutex::new(Instant::now())));
}

struct TestDependencies;

impl Dependencies for TestDependencies {
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

    type Dependencies = TestDependencies;

    type CheckOut = CheckOut<Self>;

    type Error = ();

    type CreateFuture = Ready<Result<Self::Resource, Self::Error>>;

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

    type RecycleFuture = Ready<Result<Option<Self::Resource>, Self::Error>>;

    fn recycle(&self, mut resource: Self::Resource) -> Self::RecycleFuture {
        resource.recycle_count += 1;
        ok(Some(resource))
    }
}

fn init_dependencies() {
    NOW.with(|thread_now| {
        thread_now.replace(Arc::new(Mutex::new(Instant::now())));
    })
}

trait ExpectedTraits: Clone + Send {}

impl ExpectedTraits for Pool<TestManager> {}

#[tokio::test]
async fn check_out_one_resource() {
    init_dependencies();

    let builder = Builder::new();
    let pool = builder.build(4, TestManager::new());

    let check_out = pool.check_out().await.unwrap();
    assert_eq!(check_out.id, 0);
}

#[tokio::test]
async fn check_out_all_resources() {
    init_dependencies();

    let builder = Builder::new();
    let pool = builder.build(4, TestManager::new());

    let check_out_0 = pool.check_out().await.unwrap();
    let check_out_1 = pool.check_out().await.unwrap();
    let check_out_2 = pool.check_out().await.unwrap();
    let check_out_3 = pool.check_out().await.unwrap();

    assert_eq!(check_out_0.id, 0);
    assert_eq!(check_out_1.id, 1);
    assert_eq!(check_out_2.id, 2);
    assert_eq!(check_out_3.id, 3);
}

#[tokio::test]
async fn check_out_more_resources() {
    init_dependencies();

    let builder = Builder::new();
    let pool = builder.build(4, TestManager::new());

    let check_out_0 = pool.check_out().await.unwrap();
    let _check_out_1 = pool.check_out().await.unwrap();
    let _check_out_2 = pool.check_out().await.unwrap();
    let _check_out_3 = pool.check_out().await.unwrap();

    let check_out_future = pool.check_out();
    futures::pin_mut!(check_out_future);
    assert!(try_poll(check_out_future.as_mut()).await.is_none());
    mem::drop(check_out_0);
    assert!(try_poll(check_out_future.as_mut()).await.is_some());
}

#[tokio::test]
async fn recycle_resource() {
    init_dependencies();

    let builder = Builder::new();
    let pool = builder.build(4, TestManager::new());

    for _ in 0..2u32 {
        let resource = pool.check_out().await.unwrap();
        assert_eq!(0, resource.id);
        assert_eq!(0, resource.recycle_count);
    }

    NOW.with(|now| {
        let now = now.borrow();
        let mut now = now.lock().unwrap();
        *now += Duration::from_secs(60);
    });

    for _ in 0..2u32 {
        let resource = pool.check_out().await.unwrap();
        assert_eq!(0, resource.id);
        assert_eq!(1, resource.recycle_count);
    }
}

async fn try_poll<F: Future>(f: Pin<&mut F>) -> Option<F::Output> {
    ReturnsImmediately(f).await
}

struct ReturnsImmediately<'f, F>(Pin<&'f mut F>);

impl<'f, F: Future> Future for ReturnsImmediately<'f, F> {
    type Output = Option<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match self.0.as_mut().poll(cx) {
            Poll::Ready(result) => Poll::Ready(Some(result)),
            Poll::Pending => Poll::Ready(None),
        }
    }
}
