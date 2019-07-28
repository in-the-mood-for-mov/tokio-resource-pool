use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_queue::{ArrayQueue, PushError};
use futures::future::{ok, Either, FutureResult};
use futures::{try_ready, Async, Future, Poll};
use tokio_sync::semaphore::{self, Semaphore};

use crate::machine::{Machine, State, Turn};
use crate::resource::{Idle, Manage, Status};

pub trait Dependencies {
    fn now() -> Instant;
}

pub enum RealDependencies {}

impl Dependencies for RealDependencies {
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
    shared: Option<Arc<Shared<M>>>,
}

impl<M> CheckOut<M>
where
    M: Manage,
{
    fn new(resource: M::Resource, recycled_at: Instant, shared: Arc<Shared<M>>) -> Self {
        Self {
            resource: Some(resource),
            recycled_at,
            shared: Some(shared),
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
            shared: self.shared.take(),
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
        let (resource, shared) =
            if let (Some(resource), Some(shared)) = (self.resource.take(), self.shared.take()) {
                (resource, shared)
            } else {
                return;
            };

        match shared.manager.status(&resource) {
            Status::Valid => {
                let idle_resource = Idle::new(resource, self.recycled_at);
                match shared.shelf.push(idle_resource) {
                    Ok(_) => shared.shelf_size.add_permits(1),
                    Err(PushError(_)) => {
                        log::error!(
                            "encountered a full channel while returning a resource to the pool"
                        );
                        shared.shelf_free_space.add_permits(1);
                    }
                }
            }
            Status::Invalid => {
                shared.notify_of_lost_resource();
                return;
            }
        };
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
    shared: Option<Arc<Shared<M>>>,
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
                shared: self.shared.take(),
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
        if let Some(shared) = self.shared.take() {
            shared.notify_of_lost_resource()
        }
    }
}

/// The shared state of the resource pool.
pub struct Shared<M>
where
    M: Manage,
{
    /// The total capacity if the pool.
    ///
    /// You can deduce the number of checked out resources by computing `capacity - shelf_size -
    /// shelf_free_space`.
    pub capacity: usize,

    /// Resources are put back on the shelf when they are returned.
    pub shelf: ArrayQueue<Idle<M::Resource>>,

    /// The number of items in the shelf.
    ///
    /// You can read it directly from the shelf, but consumers can wait on it when the shelf is
    /// empty.
    pub shelf_size: Semaphore,

    /// The number of resources that can still be created.
    pub shelf_free_space: Semaphore,

    pub recycle_interval: Duration,

    pub manager: M,
}

impl<M> Shared<M>
where
    M: Manage,
{
    pub fn notify_of_lost_resource(&self) {
        self.shelf_free_space.add_permits(1);
    }

    pub fn recycle(&self, resource: Idle<M::Resource>) -> M::RecycleFuture {
        self.manager.recycle(resource.into_resource())
    }

    pub fn is_stale(&self, idle_resource: &Idle<M::Resource>) -> bool {
        M::Dependencies::now() > idle_resource.recycled_at() + self.recycle_interval
    }
}

/// A handle to a pool through which resources are requested.
pub struct Pool<M>
where
    M: Manage,
{
    shared: Arc<Shared<M>>,
}

impl<M> Pool<M>
where
    M: Manage,
    M::Resource: Send,
{
    pub fn check_out(&self) -> CheckOutFuture<M> {
        let mut permit = semaphore::Permit::new();
        let inner = match permit.try_acquire(&self.shared.shelf_size) {
            Ok(()) => {
                let idle_resource = self.shared.shelf.pop().unwrap();
                if self.shared.is_stale(&idle_resource) {
                    let context = CheckOutContext {
                        shared: Arc::clone(&self.shared),
                    };
                    let future = self.shared.recycle(idle_resource);
                    let machine = Machine::new(CheckOutState::Recycling { future }, context);
                    Either::A(machine)
                } else {
                    let recycled_at = idle_resource.recycled_at();
                    let resource = idle_resource.into_resource();
                    let entry =
                        CheckOut::new(resource, recycled_at, Arc::clone(&self.shared)).into();
                    Either::B(ok(entry))
                }
            }

            Err(ref error) if error.is_no_permits() => {
                let context = CheckOutContext {
                    shared: Arc::clone(&self.shared),
                };
                let machine = Machine::new(CheckOutState::start(), context);
                Either::A(machine)
            }

            Err(ref error) => {
                // At the time of writing this, the only possibility here is that someone closed the
                // semaphore, which does not happen.
                panic!("fatal: {}", error)
            }
        };
        CheckOutFuture { inner }
    }

    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    pub fn recycle_interval(&self) -> Duration {
        self.shared.recycle_interval
    }
}

impl<M> Clone for Pool<M>
where
    M: Manage,
{
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

type CheckOutStateMachine<M> = Machine<CheckOutState<M>>;
type ImmediatelyAvailable<M> = FutureResult<<M as Manage>::CheckOut, <M as Manage>::Error>;
type CheckOutFutureInner<M> = Either<CheckOutStateMachine<M>, ImmediatelyAvailable<M>>;

/// A `Future` that will yield a resource from the pool on completion.
#[must_use = "futures do nothing unless polled"]
pub struct CheckOutFuture<M>
where
    M: Manage,
{
    inner: CheckOutFutureInner<M>,
}

impl<M> Future for CheckOutFuture<M>
where
    M: Manage,
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
    shared: Arc<Shared<M>>,
}

enum CheckOutState<M>
where
    M: Manage,
{
    Start {
        shelf_size_ticket: semaphore::Permit,
        shelf_free_space_ticket: semaphore::Permit,
    },
    Creating {
        future: M::CreateFuture,
    },
    Recycling {
        future: M::RecycleFuture,
    },
}

impl<M> CheckOutState<M>
where
    M: Manage,
{
    fn start() -> Self {
        CheckOutState::Start {
            shelf_size_ticket: semaphore::Permit::new(),
            shelf_free_space_ticket: semaphore::Permit::new(),
        }
    }

    fn on_start(
        mut shelf_size_ticket: semaphore::Permit,
        mut shelf_free_space_ticket: semaphore::Permit,
        context: &mut CheckOutContext<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        if let Async::Ready(()) = shelf_size_ticket
            .poll_acquire(&context.shared.shelf_size)
            .unwrap()
        {
            shelf_size_ticket.forget();
            shelf_free_space_ticket.forget();
            let idle_resource = context.shared.shelf.pop().unwrap();
            if context.shared.is_stale(&idle_resource) {
                let future = context.shared.recycle(idle_resource);
                return Ok(Turn::Continue(CheckOutState::Recycling { future }));
            } else {
                return Ok(Turn::Done(idle_resource));
            }
        }

        if let Async::Ready(()) = shelf_free_space_ticket
            .poll_acquire(&context.shared.shelf_free_space)
            .unwrap()
        {
            shelf_size_ticket.forget();
            shelf_free_space_ticket.forget();
            let future = context.shared.manager.create();
            return Ok(Turn::Continue(CheckOutState::Creating { future }));
        }

        Ok(Turn::Suspend(CheckOutState::Start {
            shelf_size_ticket,
            shelf_free_space_ticket,
        }))
    }

    fn on_creating(
        mut future: M::CreateFuture,
        _context: &mut CheckOutContext<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        match future.poll()? {
            Async::NotReady => Ok(Turn::Suspend(CheckOutState::Creating { future })),
            Async::Ready(resource) => Ok(Turn::Done(Idle::new(resource, M::Dependencies::now()))),
        }
    }

    fn on_recycling(
        mut future: M::RecycleFuture,
        context: &mut CheckOutContext<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        match future.poll() {
            Ok(Async::NotReady) => Ok(Turn::Suspend(CheckOutState::Recycling { future })),
            Ok(Async::Ready(Some(resource))) => {
                Ok(Turn::Done(Idle::new(resource, M::Dependencies::now())))
            }
            Ok(Async::Ready(None)) | Err(_) => {
                context.shared.notify_of_lost_resource();
                Ok(Turn::Continue(CheckOutState::start()))
            }
        }
    }
}

impl<M> State for CheckOutState<M>
where
    M: Manage,
{
    type Final = Idle<M::Resource>;

    type Item = M::CheckOut;

    type Error = M::Error;

    type Context = CheckOutContext<M>;

    fn turn(state: Self, context: &mut Self::Context) -> Result<Turn<Self>, Self::Error> {
        match state {
            CheckOutState::Start {
                shelf_size_ticket,
                shelf_free_space_ticket,
                ..
            } => Self::on_start(shelf_size_ticket, shelf_free_space_ticket, context),
            CheckOutState::Creating { future } => Self::on_creating(future, context),
            CheckOutState::Recycling { future } => Self::on_recycling(future, context),
        }
    }

    fn finalize(resource: Self::Final, context: Self::Context) -> Result<Self::Item, Self::Error> {
        let recycled_at = resource.recycled_at();
        Ok(CheckOut::new(resource.into_resource(), recycled_at, context.shared).into())
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

    pub fn build<M>(self, capacity: usize, manager: M) -> Pool<M>
    where
        M: Manage,
    {
        let shared = Arc::new(Shared {
            capacity,
            shelf: ArrayQueue::new(capacity),
            shelf_size: Semaphore::new(0),
            shelf_free_space: Semaphore::new(capacity),
            manager,
            recycle_interval: self.recycle_interval,
        });

        Pool {
            shared: shared.clone(),
        }
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}
