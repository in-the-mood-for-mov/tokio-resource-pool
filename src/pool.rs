use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::{Async, Future, Poll, try_ready};
use futures::future::{Either, FutureResult, ok};
use futures::sync::mpsc;
use tokio_sync::semaphore;

use crate::librarian::Librarian;
use crate::machine::{Machine, State, Turn};
use crate::resource::{Idle, Manage};
use crate::shared::Shared;

/// A check out of a resource from a `Pool`. The resource is automatically returned when the
/// `CheckOut` is dropped.
pub struct CheckOut<M>
where
    M: Manage,
{
    resource: Option<M::Resource>,
    pool: Option<Pool<M>>,
}

impl<M> CheckOut<M>
where
    M: Manage,
{
    fn new(resource: M::Resource, pool: Pool<M>) -> Self {
        Self {
            resource: Some(resource),
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
        if let (Some(resource), Some(mut pool)) = (self.resource.take(), self.pool.take()) {
            pool.return_chute.try_send(Idle::new(resource)).unwrap();
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
                pool: self.pool.take(),
            }.into(),
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
            pool.shared.created_count.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// The handle to the pool through which resources are requested.
pub struct Pool<M>
where
    M: Manage,
    M::Resource: Send,
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
    /// Create a new pool, with its associated `Librarian`.
    pub fn new(capacity: usize, manager: M) -> (Self, Librarian<M>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let shared = Arc::new(Shared::new(capacity, manager));
        let librarian = Librarian::new(shared.clone(), receiver);
        let pool = Self {
            shared,
            return_chute: sender,
        };
        (pool, librarian)
    }

    /// Starts the process of checking out a resource.
    pub fn check_out(&self) -> CheckOutFuture<M>
    {
        let mut permit = semaphore::Permit::new();
        let inner = if let Ok(()) = permit.try_acquire(&self.shared.checked_out_count) {
            let idle_resource = self.shared.shelf.pop().unwrap();
            let resource = idle_resource.into_resource();
            let entry = CheckOut::new(resource, self.clone()).into();
            Either::B(ok(entry))
        } else {
            let machine = Machine::new(CheckOutState::Start, self.clone());
            Either::A(machine)
        };
        CheckOutFuture { inner }
    }
}

impl<M> Clone for Pool<M>
where
    M: Manage,
    M::Resource: Send,
{
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            return_chute: self.return_chute.clone(),
        }
    }
}

/// A `Future` that will yield a resource from the pool on completion.
#[must_use = "futures do nothing unless polled"]
pub struct CheckOutFuture<M>
where
    M: Manage,
{
    inner: Either<Machine<CheckOutState<M>>, FutureResult<M::CheckOut, M::Error>>,
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

enum CheckOutState<M>
where
    M: Manage,
{
    Start,
    Creating { future: M::Future },
    Wait { permit: semaphore::Permit },
}

impl<M> CheckOutState<M>
where
    M: Manage,
{
    fn on_start(context: &mut Pool<M>) -> Result<Turn<Self>, <Self as State>::Error> {
        loop {
            let created_count = context.shared.created_count.load(Ordering::SeqCst);
            if created_count == context.shared.capacity {
                return Ok(Turn::Continue(CheckOutState::Wait {
                    permit: semaphore::Permit::new(),
                }));
            }

            let swap_result = context.shared.created_count.compare_and_swap(
                created_count,
                created_count + 1,
                Ordering::SeqCst,
            );
            if created_count == swap_result {
                let future = context.shared.manager.create();
                return Ok(Turn::Continue(CheckOutState::Creating { future }));
            }
        }
    }

    fn on_creating(
        mut future: M::Future,
        _context: &mut Pool<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        match future.poll()? {
            Async::NotReady => Ok(Turn::Suspend(CheckOutState::Creating { future })),
            Async::Ready(resource) => Ok(Turn::Done(resource)),
        }
    }

    fn on_wait(
        mut permit: semaphore::Permit,
        context: &mut Pool<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        match permit
            .poll_acquire(&context.shared.checked_out_count)
            .unwrap()
        {
            Async::NotReady => Ok(Turn::Suspend(CheckOutState::Wait { permit })),
            Async::Ready(()) => Ok(Turn::Done(
                context.shared.shelf.pop().unwrap().into_resource(),
            )),
        }
    }
}

impl<M> State for CheckOutState<M>
where
    M: Manage,
{
    type Final = M::Resource;

    type Item = M::CheckOut;

    type Error = M::Error;

    type Context = Pool<M>;

    fn turn(state: Self, context: &mut Self::Context) -> Result<Turn<Self>, Self::Error> {
        match state {
            CheckOutState::Start => Self::on_start(context),

            CheckOutState::Creating { future } => Self::on_creating(future, context),

            CheckOutState::Wait { permit } => Self::on_wait(permit, context),
        }
    }

    fn finalize(fin: Self::Final, pool: Self::Context) -> Result<Self::Item, Self::Error> {
        Ok(CheckOut::new(fin, pool).into())
    }
}
