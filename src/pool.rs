use std::ops::{Deref, DerefMut};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::sync::mpsc;
use futures::{Async, Future, Poll};
use tokio_sync::semaphore;

use crate::librarian::Librarian;
use crate::machine::{Machine, State, Turn};
use crate::resource::{Idle, Manage};
use crate::shared::Shared;
use futures::future::{ok, Either, FutureResult};

/// A check out of a resource from a `Pool`. The resource is automatically returned when the
/// `CheckOut` is dropped.
#[derive(Debug)]
pub struct CheckOut<R>
where
    R: Send,
{
    resource: Option<R>,
    sender: mpsc::Sender<Idle<R>>,
}

impl<R> CheckOut<R>
where
    R: Send,
{
    pub fn new(resource: R, sender: mpsc::Sender<Idle<R>>) -> Self {
        let resource = Some(resource);
        Self { resource, sender }
    }
}

impl<R> Deref for CheckOut<R>
where
    R: Send,
{
    type Target = R;

    fn deref(&self) -> &Self::Target {
        self.resource.as_ref().unwrap()
    }
}

impl<R> DerefMut for CheckOut<R>
where
    R: Send,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.resource.as_mut().unwrap()
    }
}

impl<R> Drop for CheckOut<R>
where
    R: Send,
{
    fn drop(&mut self) {
        let resource = self.resource.take().unwrap();
        self.sender.try_send(Idle::new(resource)).unwrap();
    }
}

/// The handle to the pool through which resources are requested.
#[derive(Clone)]
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
    pub fn check_out(&self) -> CheckOutFuture<M> {
        let mut permit = semaphore::Permit::new();
        let inner = if let Ok(()) = permit.try_acquire(&self.shared.checked_out_count) {
            let idle_resource = self.shared.shelf.pop().unwrap();
            let resource = idle_resource.into_resource();
            let entry = CheckOut::new(resource, self.return_chute.clone());
            Either::B(ok(entry))
        } else {
            let context = CheckOutContext {
                shared: Arc::clone(&self.shared),
                return_chute: self.return_chute.clone(),
            };
            let machine = Machine::new(CheckOutState::Start, context);
            Either::A(machine)
        };
        CheckOutFuture { inner }
    }
}

/// A `Future` that will yield a resource from the pool on completion.
#[must_use = "futures do nothing unless polled"]
pub struct CheckOutFuture<M>
where
    M: Manage,
{
    inner: Either<Machine<CheckOutState<M>>, FutureResult<CheckOut<M::Resource>, M::Error>>,
}

impl<M> Future for CheckOutFuture<M>
where
    M: Manage,
{
    type Item = CheckOut<M::Resource>;

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
    return_chute: mpsc::Sender<Idle<M::Resource>>,
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
    fn on_start(context: &mut CheckOutContext<M>) -> Result<Turn<Self>, <Self as State>::Error> {
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
        _context: &mut CheckOutContext<M>,
    ) -> Result<Turn<Self>, <Self as State>::Error> {
        match future.poll()? {
            Async::NotReady => Ok(Turn::Suspend(CheckOutState::Creating { future })),
            Async::Ready(resource) => Ok(Turn::Done(resource)),
        }
    }

    fn on_wait(
        mut permit: semaphore::Permit,
        context: &mut CheckOutContext<M>,
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

    type Item = CheckOut<M::Resource>;

    type Error = M::Error;

    type Context = CheckOutContext<M>;

    fn turn(state: Self, context: &mut Self::Context) -> Result<Turn<Self>, Self::Error> {
        match state {
            CheckOutState::Start => Self::on_start(context),

            CheckOutState::Creating { future } => Self::on_creating(future, context),

            CheckOutState::Wait { permit } => Self::on_wait(permit, context),
        }
    }

    fn finalize(fin: Self::Final, context: Self::Context) -> Result<Self::Item, Self::Error> {
        Ok(CheckOut::new(fin, context.return_chute))
    }
}
