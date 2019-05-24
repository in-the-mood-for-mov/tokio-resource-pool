use futures::Future;

use crate::CheckOut;

/// A trait for managing the lifecycle of a resource.
pub trait Manage: Sized {
    type Resource: Send;

    type CheckOut: From<CheckOut<Self>>;

    type Error;

    type Future: Future<Item = Self::Resource, Error = Self::Error>;

    fn create(&self) -> Self::Future;
}

#[derive(Debug)]
pub struct Idle<R>
where
    R: Send,
{
    resource: R,
}

impl<R> Idle<R>
where
    R: Send,
{
    pub fn new(resource: R) -> Self {
        Self { resource }
    }

    pub fn into_resource(self) -> R {
        self.resource
    }
}
