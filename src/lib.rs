//! sabun: a small incremental view maintenance engine.
//!
//! The whole engine rests on one idea: a collection and a change to a
//! collection are the same kind of thing. See [`zset::ZSet`].

pub mod aggregate;
pub mod circuit;
pub mod distinct;
pub mod join;
pub mod reachability;
pub mod zset;

#[cfg(test)]
pub(crate) mod checker;
