use std::sync::atomic::AtomicUsize;

use crossbeam_queue::ArrayQueue;
use tokio_sync::semaphore::Semaphore;

use crate::resource::Idle;
use crate::Manage;

pub struct Shared<M>
where
    M: Manage,
{
    pub capacity: usize,

    /// Resources on the shelf are ready to be borrowed. You must increment `checked_out_count`
    /// before grabbing one though.
    pub shelf: ArrayQueue<Idle<M::Resource>>,

    pub checked_out_count: Semaphore,

    pub created_count: AtomicUsize,

    pub manager: M,
}

impl<M> Shared<M>
where
    M: Manage,
{
    pub fn new(capacity: usize, manager: M) -> Self {
        Self {
            capacity,
            shelf: ArrayQueue::new(capacity),
            checked_out_count: Semaphore::new(0),
            created_count: AtomicUsize::new(0),
            manager,
        }
    }
}
