//! Cross-replica state. The in-memory limiters and pins are correct for one
//! process; with several replicas behind one address they are each N times
//! too generous and blind to each other's pins. `[cluster] backend = "redis"`
//! moves them onto one Redis, reached through [`RedisLink`].

pub mod link;

pub use link::RedisLink;
