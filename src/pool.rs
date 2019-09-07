use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crossbeam_queue::{ArrayQueue, PushError};
use futures::future::{select, Either};
use futures::ready;
use tokio_sync::semaphore::{self, Semaphore};

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
    pub async fn lend<F, T, B, E>(mut self, borrower: B) -> LentCheckOut<M, F, T, E>
    where
        B: FnOnce(M::Resource) -> F,
        F: Future<Output = Result<(M::Resource, T), E>>,
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
            }
        };
    }
}

/// A future where a resource is lent to an opaque, asynchronous computation.
pub struct LentCheckOut<M, F, T, E>
where
    M: Manage,
    F: Future<Output = Result<(M::Resource, T), E>>,
{
    inner: F,
    recycled_at: Instant,
    shared: Option<Arc<Shared<M>>>,
}

impl<M, F, T, E> LentCheckOut<M, F, T, E>
where
    M: Manage,
    F: Future<Output = Result<(M::Resource, T), E>>,
{
    fn inner(self: Pin<&mut Self>) -> Pin<&mut F> {
        // Safety: pinning is structural for `inner`, i.e. `inner` is pinned when `self` is.
        // 1. `Self` does not implement `Unpin` if `inner` does.
        // 2. The `Drop` impl does not move out of `inner`.
        // 3. The `Drop` impl cannot panic before invoking `inner`'s `Drop` impl.
        // 4. No other function can move out of `inner`.
        unsafe { self.map_unchecked_mut(|l| &mut l.inner) }
    }

    fn shared(self: Pin<&mut Self>) -> &mut Option<Arc<Shared<M>>> {
        // Safety: pinning is not structural for `shared`.
        // 1. It is impossible to create a pinned pointer to this field.
        unsafe { &mut self.get_unchecked_mut().shared }
    }
}

impl<M, F, T, E> Future for LentCheckOut<M, F, T, E>
where
    M: Manage,
    F: Future<Output = Result<(M::Resource, T), E>>,
{
    type Output = Result<(M::CheckOut, T), E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let (resource, t) = ready!(self.as_mut().inner().poll(cx))?;
        Poll::Ready(Ok((
            CheckOut {
                resource: Some(resource),
                recycled_at: self.recycled_at,
                shared: self.as_mut().shared().take(),
            }
            .into(),
            t,
        )))
    }
}

impl<M, F, T, E> Drop for LentCheckOut<M, F, T, E>
where
    M: Manage,
    F: Future<Output = Result<(M::Resource, T), E>>,
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
    pub async fn check_out(&self) -> Result<M::CheckOut, M::Error> {
        loop {
            if let Some(checkout) = self.check_out_once().await? {
                return Ok(checkout);
            }
        }
    }

    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    pub fn recycle_interval(&self) -> Duration {
        self.shared.recycle_interval
    }

    async fn check_out_once(&self) -> Result<Option<M::CheckOut>, M::Error> {
        let mut permit = semaphore::Permit::new();
        match permit.try_acquire(&self.shared.shelf_size) {
            Ok(()) => self.pop_resource().await,

            Err(ref error) => {
                // At the time of writing this, the only possibility here is that someone closed the
                // semaphore, which must not happen.
                assert!(error.is_no_permits(), "fatal: {}", error);
                self.wait_for_checkout().await
            }
        }
    }

    async fn wait_for_checkout(&self) -> Result<Option<M::CheckOut>, M::Error> {
        let shelf_size_permit = WaitOnSemaphore::new(&self.shared.shelf_size);
        let shelf_free_space_permit = WaitOnSemaphore::new(&self.shared.shelf_free_space);
        match select(shelf_size_permit, shelf_free_space_permit).await {
            Either::Left((mut shelf_size_permit, _)) => {
                shelf_size_permit.forget();
                self.pop_resource().await
            }
            Either::Right((mut shelf_free_space_permit, _)) => {
                shelf_free_space_permit.forget();
                let resource = self.shared.manager.create().await?;
                let recycled_at = M::Dependencies::now();
                let shared = Arc::clone(&self.shared);
                Ok(Some(CheckOut::new(resource, recycled_at, shared).into()))
            }
        }
    }

    async fn pop_resource(&self) -> Result<Option<M::CheckOut>, M::Error> {
        let idle_resource = self.shared.shelf.pop().unwrap();
        if self.shared.is_stale(&idle_resource) {
            self.recycle(idle_resource).await
        } else {
            let recycled_at = idle_resource.recycled_at();
            let resource = idle_resource.into_resource();
            let shared = Arc::clone(&self.shared);
            Ok(Some(CheckOut::new(resource, recycled_at, shared).into()))
        }
    }

    async fn recycle(&self, resource: Idle<M::Resource>) -> Result<Option<M::CheckOut>, M::Error> {
        Ok(self.shared.recycle(resource).await?.map(|resource| {
            let recycled_at = M::Dependencies::now();
            let shared = Arc::clone(&self.shared);
            let checkout = CheckOut::new(resource, recycled_at, shared);
            checkout.into()
        }))
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

struct WaitOnSemaphore<'s> {
    semaphore: &'s Semaphore,
    permit: Option<semaphore::Permit>,
}

impl<'s> WaitOnSemaphore<'s> {
    fn new(semaphore: &'s Semaphore) -> Self {
        Self {
            semaphore,
            permit: Some(semaphore::Permit::new()),
        }
    }
}

impl<'s> Future for WaitOnSemaphore<'s> {
    type Output = semaphore::Permit;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut permit = self.permit.take().unwrap();
        match permit.poll_acquire(cx, &self.semaphore) {
            Poll::Pending => {
                self.permit = Some(permit);
                Poll::Pending
            }
            Poll::Ready(Ok(())) => Poll::Ready(permit),
            Poll::Ready(Err(_)) => panic!(),
        }
    }
}

impl<'s> Drop for WaitOnSemaphore<'s> {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.as_mut() {
            permit.forget();
        }
    }
}

//type CheckOutFuture<M: Manage> = impl Future<Output = Result<Idle<M::Resource>, M::Error>>;

//type CheckOutStateMachine<M> = Machine<CheckOutState<M>>;
//type ImmediatelyAvailable<M> = FutureResult<<M as Manage>::CheckOut, <M as Manage>::Error>;
//type CheckOutFutureInner<M> = Either<CheckOutStateMachine<M>, ImmediatelyAvailable<M>>;
//
///// A `Future` that will yield a resource from the pool on completion.
//#[must_use = "futures do nothing unless polled"]
//pub struct CheckOutFuture<M>
//where
//    M: Manage,
//{
//    inner: CheckOutFutureInner<M>,
//}
//
//impl<M> Future for CheckOutFuture<M>
//where
//    M: Manage,
//{
//    type Item = M::CheckOut;
//
//    type Error = M::Error;
//
//    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//        self.inner.poll()
//    }
//}
//
//struct CheckOutContext<M>
//where
//    M: Manage,
//{
//    shared: Arc<Shared<M>>,
//}
//
//enum CheckOutState<M>
//where
//    M: Manage,
//{
//    Start {
//        shelf_size_ticket: semaphore::Permit,
//        shelf_free_space_ticket: semaphore::Permit,
//    },
//    Creating {
//        future: M::CreateFuture,
//    },
//    Recycling {
//        future: M::RecycleFuture,
//    },
//}
//
//impl<M> CheckOutState<M>
//where
//    M: Manage,
//{
//    fn start() -> Self {
//        CheckOutState::Start {
//            shelf_size_ticket: semaphore::Permit::new(),
//            shelf_free_space_ticket: semaphore::Permit::new(),
//        }
//    }
//
//    fn on_start(
//        mut shelf_size_ticket: semaphore::Permit,
//        mut shelf_free_space_ticket: semaphore::Permit,
//        context: &mut CheckOutContext<M>,
//    ) -> Result<Turn<Self>, <Self as State>::Error> {
//        if let Async::Ready(()) = shelf_size_ticket
//            .poll_acquire(&context.shared.shelf_size)
//            .unwrap()
//        {
//            shelf_size_ticket.forget();
//            shelf_free_space_ticket.forget();
//            let idle_resource = context.shared.shelf.pop().unwrap();
//            if context.shared.is_stale(&idle_resource) {
//                let future = context.shared.recycle(idle_resource);
//                return Ok(Turn::Continue(CheckOutState::Recycling { future }));
//            } else {
//                return Ok(Turn::Done(idle_resource));
//            }
//        }
//
//        if let Async::Ready(()) = shelf_free_space_ticket
//            .poll_acquire(&context.shared.shelf_free_space)
//            .unwrap()
//        {
//            shelf_size_ticket.forget();
//            shelf_free_space_ticket.forget();
//            let future = context.shared.manager.create();
//            return Ok(Turn::Continue(CheckOutState::Creating { future }));
//        }
//
//        Ok(Turn::Suspend(CheckOutState::Start {
//            shelf_size_ticket,
//            shelf_free_space_ticket,
//        }))
//    }
//
//    fn on_creating(
//        mut future: M::CreateFuture,
//        _context: &mut CheckOutContext<M>,
//    ) -> Result<Turn<Self>, <Self as State>::Error> {
//        match future.poll()? {
//            Async::NotReady => Ok(Turn::Suspend(CheckOutState::Creating { future })),
//            Async::Ready(resource) => Ok(Turn::Done(Idle::new(resource, M::Dependencies::now()))),
//        }
//    }
//
//    fn on_recycling(
//        mut future: M::RecycleFuture,
//        context: &mut CheckOutContext<M>,
//    ) -> Result<Turn<Self>, <Self as State>::Error> {
//        match future.poll() {
//            Ok(Async::NotReady) => Ok(Turn::Suspend(CheckOutState::Recycling { future })),
//            Ok(Async::Ready(Some(resource))) => {
//                Ok(Turn::Done(Idle::new(resource, M::Dependencies::now())))
//            }
//            Ok(Async::Ready(None)) | Err(_) => {
//                context.shared.notify_of_lost_resource();
//                Ok(Turn::Continue(CheckOutState::start()))
//            }
//        }
//    }
//}
//
//impl<M> State for CheckOutState<M>
//where
//    M: Manage,
//{
//    type Final = Idle<M::Resource>;
//
//    type Item = M::CheckOut;
//
//    type Error = M::Error;
//
//    type Context = CheckOutContext<M>;
//
//    fn turn(state: Self, context: &mut Self::Context) -> Result<Turn<Self>, Self::Error> {
//        match state {
//            CheckOutState::Start {
//                shelf_size_ticket,
//                shelf_free_space_ticket,
//                ..
//            } => Self::on_start(shelf_size_ticket, shelf_free_space_ticket, context),
//            CheckOutState::Creating { future } => Self::on_creating(future, context),
//            CheckOutState::Recycling { future } => Self::on_recycling(future, context),
//        }
//    }
//
//    fn finalize(resource: Self::Final, context: Self::Context) -> Result<Self::Item, Self::Error> {
//        let recycled_at = resource.recycled_at();
//        Ok(CheckOut::new(resource.into_resource(), recycled_at, context.shared).into())
//    }
//}

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
