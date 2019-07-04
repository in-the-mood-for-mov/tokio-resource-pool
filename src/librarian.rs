use std::sync::Arc;

use futures::stream::Stream;
use futures::sync::mpsc;
use futures::{Async, Future, Poll};

use crate::pool::Shared;
use crate::resource::Idle;
use crate::Manage;

/// A `Future` that manages the internal state of a `Pool`.
///
/// It is created alongside a `Pool` when calling `Pool::new` and must be spawned on a runtime to do
/// its job. It automatically exits when all clones of a `Pool` are dropped.
#[must_use = "futures do nothing unless polled"]
pub struct Librarian<M>
where
    M: Manage,
{
    pool: Arc<Shared<M>>,
    receiver: mpsc::Receiver<Idle<M::Resource>>,
}

impl<M> Librarian<M>
where
    M: Manage,
{
    pub(crate) fn new(pool: Arc<Shared<M>>, receiver: mpsc::Receiver<Idle<M::Resource>>) -> Self {
        Self { pool, receiver }
    }
}

impl<M> Future for Librarian<M>
where
    M: Manage,
{
    type Item = ();

    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.receiver.poll() {
                Err(()) => panic!(),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(None)) => return Ok(Async::Ready(())),
                Ok(Async::Ready(Some(idle_resource))) => {
                    self.pool.shelf.push(idle_resource).unwrap();
                    self.pool.checked_out_count.add_permits(1);
                }
            }
        }
    }
}
