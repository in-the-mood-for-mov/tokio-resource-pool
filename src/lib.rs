//! A generic resource pool for the Tokio ecosystem.
//!
//! # Example
//!
//! To use it, you need to implment `Manage` for your resource, and then create a `Pool` with its
//! background worker, spawn the worker. Once this is done, you can request a resource by calling
//! `Pool::check_out`.
//!
//! ```
//! use futures::future::{lazy, Future, FutureResult, IntoFuture};
//! use redis::{RedisError, RedisResult};
//! use tokio;
//!
//! use tokio_resource_pool::{Manage, Pool};
//!
//! struct RedisConnectionManager {
//!     client: redis::Client,
//! }
//!
//! impl RedisConnectionManager {
//!     fn new(url: impl redis::IntoConnectionInfo) -> RedisResult<Self> {
//!         let client = redis::Client::open(url)?;
//!         Ok(Self { client })
//!     }
//! }
//!
//! impl Manage for RedisConnectionManager {
//!     type Resource = redis::Connection;
//!
//!     type Error = RedisError;
//!
//!     type Future = FutureResult<Self::Resource, Self::Error>;
//!
//!     fn create(&self) -> Self::Future {
//!         self.client.get_connection().into_future()
//!     }
//! }
//!
//! # fn main() -> RedisResult<()> {
//! let manager = RedisConnectionManager::new("redis://127.0.0.1/")?;
//! tokio::run(lazy(move || {
//!     let (pool, librarian) = Pool::new(4, manager);
//!     tokio::spawn(librarian);
//!     tokio::spawn(
//!         pool.check_out()
//!             .and_then(|connection| {
//!                 redis::cmd("INFO").query::<redis::InfoDict>(&*connection)
//!             })
//!             .map(|info| println!("{:#?}", info))
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
pub use crate::pool::{CheckOut, CheckOutFuture, Pool};
pub use crate::resource::Manage;

mod librarian;
mod machine;
mod pool;
mod resource;
mod shared;

#[cfg(test)]
mod tests;
