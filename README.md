A generic resource pool for the Tokio ecosystem.

# Documentation

See [the documentation hosted on docs.rs](https://docs.rs/tokio-resource-pool/*/tokio_resource_pool/).

# Alternatives

There is another resource pool called [`bb8`](https://crates.io/crates/bb8). It has two significant differences.

* The API is different. This library gives you a `struct` that dereferences to your resource while `bb8` turns a closure from a resource to a `Future` that yields the resource back.

* Reaping is done differently. This library reaps resources as soon as they are returned, while `bb8` reaps them at a given interval.
