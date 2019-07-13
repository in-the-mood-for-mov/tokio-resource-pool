use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use futures::future::{ok, Either, FutureResult};
use futures::sync::mpsc;
use futures::{try_ready, Async, Future, Poll};
use tokio_sync::semaphore::{self, Semaphore};

use crate::librarian::Librarian;
use crate::machine::{Machine, State, Turn};
use crate::resource::{Idle, Manage, Status};

pub trait Env {
    fn now() -> Instant;
}

pub struct DefaultEnv;

impl Env for DefaultEnv {
    fn now() -> Instant {
        Instant::now()
    }
}

/// A check out of a resource from a `Pool`. The resource is automatically returned when the
/// `CheckOut` is dropped.
pub struct CheckOut<M>
where
    M: Manage,
{
    resource: Option<M::Resource>,
    recycled_at: Instant,
    pool: Option<Pool<M>>,
}

impl<M> CheckOut<M>
where
    M: Manage,
{
    fn new(resource: M::Resource, recycled_at: Instant, pool: Pool<M>) -> Self {
        Self {
            resource: Some(resource),
            recycled_at,
            pool: Some(pool),
        }
    }

    /// Lends the resource to an opaque asynchronous computation.
    ///
    /// Like in real life, it is usually a bad idea to lend things you don't own. If the
    /// subcomputation finishes with an error, the resource is lost. `LentCheckOut` takes care of
    /// notifying the pool of that so new resources can be created.
    ///
    /// This is necessary in situation where a resource is taken by value.
    pub fn lend<F, T, B>(mut self, borrower: B) -> LentCheckOut<M, F, T>
    where
        F: Future<Item = (M::Resource, T)>,
        B: FnOnce(M::Resource) -> F,
    {
        LentCheckOut {
            inner: borrower(self.resource.take().unwrap()),
            recycled_at: self.recycled_at,
            pool: self.pool.take(),
        }
    }
}

impl<M> Deref for CheckOut<M>
where
    M: Manage,
{
    type Target = M::Resource;

    fn deref(&self) -> &Self::Target {
        self.resource.as_ref().unwrap()
    }
}

impl<M> DerefMut for CheckOut<M>
where
    M: Manage,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.resource.as_mut().unwrap()
    }
}

impl<M> Drop for CheckOut<M>
where
    M: Manage,
{
    fn drop(&mut self) {
        let (resource, mut pool) =
            if let (Some(resource), Some(pool)) = (self.resource.take(), self.pool.take()) {
                (resource, pool)
            } else {
                return;
            };

        let error = match pool.shared.manager.status(&resource) {
            Status::Valid => {
                let result = pool
                    .return_chute
                    .try_send(Idle::new(resource, self.recycled_at));
                if let Err(error) = result {
                    pool.notify_of_lost_resource();
                    error
                } else {
                    return;
                }
            }
            Status::Invalid => {
                pool.notify_of_lost_resource();
                return;
            }
        };

        if error.is_full() {
            error!("encountered a full channel while returning a resource to the pool");
        } else if error.is_disconnected() {
            // The librarian has been dropped, this happens when the runtime is being shut down.
        } else {
            error!("{}", error);
        }
    }
}

/// A future where a resource is lent to an opaque, asynchronous computation.
pub struct LentCheckOut<M, F, T>
where
    M: Manage,
    F: Future<Item = (M::Resource, T)>,
{
    inner: F,
    recycled_at: Instant,
    pool: Option<Pool<M>>,
}

impl<M, F, T> Future for LentCheckOut<M, F, T>
where
    M: Manage,
    F: Future<Item = (M::Resource, T)>,
{
    type Item = (M::CheckOut, T);

    type Error = F::Error;

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        let (resource, t) = try_ready!(self.inner.poll());
        Ok(Async::Ready((
            CheckOut {
                resource: Some(resource),
                recycled_at: self.recycled_at,
                pool: self.pool.take(),
            }
            .into(),
            t,
        )))
    }
}

impl<M, F, T> Drop for LentCheckOut<M, F, T>
where
    M: Manage,
    F: Future<Item = (M::Resource, T)>,
{
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.notify_of_lost_resource()
        }
    }
}

pub struct Shared<M>
where
    M: Manage,
{
    pub capacity: usize,

    /// Resources on the shelf are ready to be borrowed.
    pub shelf: ArrayQueue<Idle<M::Resource>>,

    /// The number of resources on the shelf. You need to take a permit from this semaphore before
    /// taking a resource from the shelf.
    pub resources_on_shelf: Semaphore,

    pub created_count: AtomicUsize,

    pub manager: M,

    pub recycle_interval: Duration,
}

/// The handle to the pool through which resources are requested.
pub struct Pool<M>
where
    M: Manage,
{
    shared: Arc<Shared<M>>,

    /// Resources are returned by sending them into this chute. Once there, it is the librarian that
    /// will inspect the resource, then take the decision to `drop` it or put is back on the shelf.
    return_chute: mpsc::Sender<Idle<M::Resource>>,
}

impl<M> Pool<M>
where
    M: Manage,
    M::Resource: Send,
{
    /// Starts the process of checking out a resource.
    pub fn check_out(&self) -> CheckOutFuture<M, DefaultEnv> {
        self.check_out_with_environment::<DefaultEnv>()
    }

    pub fn check_out_with_environment<E: Env>(&self) -> CheckOutFuture<M, E> {
        let mut permit = semaphore::Permit::new();
        let inner = if let Ok(()) = permit.try_acquire(&self.shared.resources_on_shelf) {
            let idle_resource = self.shared.shelf.pop().unwrap();
            if self.is_stale::<E>(&idle_resource) {
                let context = CheckOutContext { pool: self.clone() };
                let future = self.recycle(idle_resource);
                let machine = Machine::new(CheckOutState::Recycling { future }, context);
                Either::A(machine)
            } else {
                let recycled_at = idle_resource.recycled_at();
                let resource = idle_resource.into_resource();
                let entry = CheckOut::new(resource, recycled_at, self.clone()).into();
                Either::B(ok(entry))
            }
        } else {
            let context = CheckOutContext { pool: self.clone() };
            let machine = Machine::new(CheckOutState::start(), context);
            Either::A(machine)
        };
        CheckOutFuture { inner }
    }

    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    pub fn recycle_interval(&self) -> Duration {
        self.shared.recycle_interval
    }

    pub(crate) fn recycle(&self, resource: Idle<M::Resource>) -> M::RecycleFuture {
        self.shared.manager.recycle(resource.into_resource())
    }

    pub(crate) fn is_stale<E: Env>(&self, idle_resource: &Idle<M::Resource>) -> bool {
        E::now() > idle_resource.recycled_at() + self.recycle_interval()
    }

    pub(crate) fn notify_of_lost_resource(&self) {
        self.shared.created_count.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<M> Clone for Pool<M>
where
    M: Manage,
{
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            return_chute: self.return_chute.clone(),
        }
    }
}

type CheckOutStateMachine<M, E> = Machine<CheckOutState<M, E>>;
type ImmediatelyAvailable<M> = FutureResult<<M as Manage>::CheckOut, <M as Manage>::Error>;
type CheckOutFutureInner<M, E> = Either<CheckOutStateMachine<M, E>, ImmediatelyAvailable<M>>;

/// A `Future` that will yield a resource from the pool on completion.
#[must_use = "futures do nothing unless polled"]
pub struct CheckOutFuture<M, E = DefaultEnv>
where
    M: Manage,
    E: Env,
{
    inner: CheckOutFutureInner<M, E>,
}

impl<M, E> Future for CheckOutFuture<M, E>
where
    M: Manage,
    E: Env,
{
    type Item = M::CheckOut;

    type Error = M::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

struct CheckOutContext<M>
where
    M: Manage,
{
    pool: Pool<M>,
}

enum CheckOutState<M, E>
where
    M: Manage,
    E: Env,
{
    Start { _environment: PhantomData<E> },
    Creating { future: M::CreateFuture },
    Wait { permit: semaphore::Permit },
    Recycling { future: M::RecycleFuture },
}

impl<M, E> CheckOutState<M, E>
where
    M: Manage,
    E: Env,
{
    fn start() -> Self {
        CheckOutState::Start {
            _environment: PhantomData,
        }
    }

    fn on_start(context: &mut CheckOutContext<M>) -> Result<Turn<Self>, <Self as State>::Error> {
        loop {
            let pool = &mut context.pool;
            let created_count = pool.shared.created_count.load(Ordering::SeqCst);
            if created_count == pool.shared.capacity {
                return Ok(Turn::Continue(CheckOutState::Wait {
                    permit: semaphore::Permit::new(),
                }));
            }

            let swap_result = pool.shared.created_count.compare_and_swap(
                created_count,
                created_count + 1,
                Ordering::SeqCst,
            );
            if created_count == swap_result {
                let future = pool.shared.manager.create();
                return Ok(Turn::Continue(CheckOutState::Creating { future }));
            }
        }
    }

    fn on_creating(
        mut future: M::CreateFuture,
        _context: &mut CheckOutContext<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        match future.poll()? {
            Async::NotReady => Ok(Turn::Suspend(CheckOutState::Creating { future })),
            Async::Ready(resource) => Ok(Turn::Done(Idle::new(resource, E::now()))),
        }
    }

    fn on_wait(
        mut permit: semaphore::Permit,
        context: &mut CheckOutContext<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        let poll = permit
            .poll_acquire(&context.pool.shared.resources_on_shelf)
            .unwrap();
        match poll {
            Async::NotReady => Ok(Turn::Suspend(CheckOutState::Wait { permit })),
            Async::Ready(()) => {
                let idle_resource = context.pool.shared.shelf.pop().unwrap();
                if context.pool.is_stale::<E>(&idle_resource) {
                    let future = context.pool.recycle(idle_resource);
                    Ok(Turn::Continue(CheckOutState::Recycling { future }))
                } else {
                    Ok(Turn::Done(idle_resource))
                }
            }
        }
    }

    fn on_recycling(
        mut future: M::RecycleFuture,
        context: &mut CheckOutContext<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        match future.poll() {
            Ok(Async::NotReady) => Ok(Turn::Suspend(CheckOutState::Recycling { future })),
            Ok(Async::Ready(Some(resource))) => Ok(Turn::Done(Idle::new(resource, E::now()))),
            Ok(Async::Ready(None)) | Err(_) => {
                context.pool.notify_of_lost_resource();
                Ok(Turn::Continue(CheckOutState::start()))
            }
        }
    }
}

impl<M, E> State for CheckOutState<M, E>
where
    M: Manage,
    E: Env,
{
    type Final = Idle<M::Resource>;

    type Item = M::CheckOut;

    type Error = M::Error;

    type Context = CheckOutContext<M>;

    fn turn(state: Self, context: &mut Self::Context) -> Result<Turn<Self>, Self::Error> {
        match state {
            CheckOutState::Start { .. } => Self::on_start(context),
            CheckOutState::Creating { future } => Self::on_creating(future, context),
            CheckOutState::Wait { permit } => Self::on_wait(permit, context),
            CheckOutState::Recycling { future } => Self::on_recycling(future, context),
        }
    }

    fn finalize(resource: Self::Final, context: Self::Context) -> Result<Self::Item, Self::Error> {
        let recycled_at = resource.recycled_at();
        Ok(CheckOut::new(resource.into_resource(), recycled_at, context.pool).into())
    }
}

pub struct Builder {
    recycle_interval: Duration,
}

impl Builder {
    pub fn new() -> Self {
        Self {
            recycle_interval: Duration::from_secs(30),
        }
    }

    pub fn recycle_interval(mut self, recycle_interval: Duration) -> Self {
        self.recycle_interval = recycle_interval;
        self
    }

    pub fn build<M>(self, capacity: usize, manager: M) -> (Pool<M>, Librarian<M>)
    where
        M: Manage,
    {
        let (sender, receiver) = mpsc::channel(capacity);

        let shared = Arc::new(Shared {
            capacity,
            shelf: ArrayQueue::new(capacity),
            resources_on_shelf: Semaphore::new(0),
            created_count: AtomicUsize::new(0),
            manager,
            recycle_interval: self.recycle_interval,
        });

        let pool = Pool {
            shared: shared.clone(),
            return_chute: sender,
        };
        let librarian = Librarian::new(shared, receiver);
        (pool, librarian)
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}
