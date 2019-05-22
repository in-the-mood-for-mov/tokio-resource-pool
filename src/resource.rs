use futures::Future;

/// A trait for managing the lifecycle of a resource.
pub trait Manage {
    type Resource: Send;

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
