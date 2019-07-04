//! A generic resource pool for the Tokio ecosystem.
//!
//! # Example
//!
//! To use it, you need to implment `Manage` for your resource, and then create a `Pool` with its
//! background worker, spawn the worker. Once this is done, you can request a resource by calling
//! `Pool::check_out`.
//!
//! ```
//! use futures::{try_ready, Async, Poll};
//! use futures::future::{lazy, Future, FutureResult, IntoFuture};
//! use redis::{RedisError, RedisFuture, RedisResult};
//! use redis::r#async::{Connection, ConnectionLike};
//! use tokio;
//!
//! use tokio_resource_pool::{Builder, CheckOut, Manage, Pool, Status};
//!
//! struct RedisManager {
//!     client: redis::Client,
//! }
//!
//! impl RedisManager {
//!     fn new(url: impl redis::IntoConnectionInfo) -> RedisResult<Self> {
//!         let client = redis::Client::open(url)?;
//!         Ok(Self { client })
//!     }
//! }
//!
//! impl Manage for RedisManager {
//!     type Resource = Connection;
//!
//!     type CheckOut = RedisCheckOut;
//!
//!     type Error = RedisError;
//!
//!     type CreateFuture = Box<dyn Future<Item = Self::Resource, Error = Self::Error> + Send>;
//!
//!     fn create(&self) -> Self::CreateFuture {
//!         Box::new(self.client.get_async_connection())
//!     }
//!
//!     fn status(&self, _: &Self::Resource) -> Status {
//!         Status::Valid
//!     }
//!
//!     type RecycleFuture = RecycleFuture;
//!
//!     fn recycle(&self, connection: Self::Resource) -> Self::RecycleFuture {
//!         let inner = redis::cmd("PING").query_async::<_, ()>(connection);
//!         RecycleFuture { inner }
//!     }
//! }
//!
//! pub struct RecycleFuture {
//!     inner: RedisFuture<(Connection, ())>,
//! }
//!
//! impl Future for RecycleFuture {
//!     type Item = Option<Connection>;
//!
//!     type Error = RedisError;
//!
//!     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
//!         let (connection, ()) = try_ready!(self.inner.poll());
//!         Ok(Async::Ready(Some(connection)))
//!     }
//! }
//!
//! pub struct RedisCheckOut {
//!     inner: CheckOut<RedisManager>,
//! }
//!
//! impl ConnectionLike for RedisCheckOut {
//!     fn req_packed_command(
//!         self,
//!         cmd: Vec<u8>,
//!     ) -> Box<dyn Future<Item = (Self, redis::Value), Error = RedisError> + Send> {
//!         let borrower = move |connection: Connection| connection.req_packed_command(cmd);
//!         Box::new(self.inner.lend(borrower))
//!     }
//!
//!     fn req_packed_commands(
//!         self,
//!         cmd: Vec<u8>,
//!         offset: usize,
//!         count: usize,
//!     ) -> Box<dyn Future<Item = (Self, Vec<redis::Value>), Error = RedisError> + Send> {
//!         let borrower =
//!             move |connection: Connection| connection.req_packed_commands(cmd, offset, count);
//!         Box::new(self.inner.lend(borrower))
//!     }
//!
//!     fn get_db(&self) -> i64 {
//!         self.inner.get_db()
//!     }
//! }
//!
//! impl From<CheckOut<RedisManager>> for RedisCheckOut {
//!     fn from(inner: CheckOut<RedisManager>) -> Self {
//!         Self { inner }
//!     }
//! }
//!
//! # fn main() -> RedisResult<()> {
//! let manager = RedisManager::new("redis://127.0.0.1/")?;
//! tokio::run(lazy(move || {
//!     let (pool, librarian) = Builder::new().build(4, manager);
//!     tokio::spawn(librarian);
//!     tokio::spawn(
//!         pool.check_out()
//!             .and_then(|connection| {
//!                 redis::cmd("INFO").query_async::<_, redis::InfoDict>(connection)
//!             })
//!             .map(|(_, info)| println!("{:#?}", info))
//!             .map_err(|error| eprintln!("error: {}", error)),
//!     )
//! }));
//! # Ok(())
//! # }
//! ```
//!
//! # Alternatives
//!
//! There is another resource pool called [`bb8`]. It has two significant differences.
//!
//! * The API is different. This library gives you a `struct` that dereferences to your resource
//!   while `bb8` turns a closure from a resource to a `Future` that yields the resource back.
//!
//! * Reaping is done differently. This library reaps resources as soon as they are returned, while
//!   `bb8` reaps them at a given interval.
//!
//! [`bb8`]: https://crates.io/crates/bb8

pub use crate::librarian::Librarian;
pub use crate::pool::{Builder, CheckOut, CheckOutFuture, LentCheckOut, Pool};
pub use crate::resource::{Manage, Status};

mod librarian;
mod machine;
mod pool;
mod resource;

#[cfg(test)]
mod tests;
