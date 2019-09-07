use std::future::Future;
use std::time::Instant;

use crate::pool::{CheckOut, Dependencies};
use crate::RealDependencies;

pub enum Status {
    Valid,
    Invalid,
}

/// A trait for managing the lifecycle of a resource.
pub trait Manage: Sized {
    type Resource: Send;

    type Dependencies: Dependencies = RealDependencies;

    type CheckOut: From<CheckOut<Self>>;

    type Error;

    type CreateFuture: Future<Output = Result<Self::Resource, Self::Error>>;

    /// Creates a new instance of the managed resource.
    fn create(&self) -> Self::CreateFuture;

    fn status(&self, resource: &Self::Resource) -> Status;

    type RecycleFuture: Future<Output = Result<Option<Self::Resource>, Self::Error>>;

    /// Recycling a resource is done periodically to determine whether it is still valid and can be
    /// reused or if it is broken and must be discarded.
    fn recycle(&self, resource: Self::Resource) -> Self::RecycleFuture;
}

#[derive(Debug)]
pub struct Idle<R>
where
    R: Send,
{
    resource: R,
    recycled_at: Instant,
}

impl<R> Idle<R>
where
    R: Send,
{
    pub fn new(resource: R, recycled_at: Instant) -> Self {
        Self {
            resource,
            recycled_at,
        }
    }

    pub fn into_resource(self) -> R {
        self.resource
    }

    pub fn recycled_at(&self) -> Instant {
        self.recycled_at
    }
}
