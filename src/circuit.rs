//! Unary operators composed into a view, fed one change at a time.
//!
//! Count, sum, min, and distinct all take one change and return one change.
//! Join does not: it takes a change from each side. That shared unary shape is
//! [`Operator`]. A pipeline of unary operators is itself an [`Operator`].
//!
//! Join stays a binary step. A view that needs it calls `Join::step`, then
//! feeds the result into a unary pipeline.
//!
//! A new view over a table that already has rows starts by replaying those
//! rows as one change. After that it takes incremental changes like any other
//! operator.

use std::convert::Infallible;
use std::error::Error as StdError;
use std::fmt;
use std::marker::PhantomData;

use crate::zset::ZSet;

/// One change in, one change out, maybe state.
///
/// This trait exists because count, sum, min, and distinct share that call
/// shape. Join is not an [`Operator`]: forcing two inputs into one change
/// would hide which side moved.
pub trait Operator {
    type In: Ord;
    type Out: Ord;
    type Error;

    fn step(&mut self, change: ZSet<Self::In>) -> Result<ZSet<Self::Out>, Self::Error>;

    /// `self` then `next`: the output change of one is the input of the other.
    fn then<B>(self, next: B) -> Then<Self, B>
    where
        Self: Sized,
        B: Operator<In = Self::Out>,
    {
        Then::new(self, next)
    }
}

/// Two unary operators in a row. Static: the type is the pipeline.
#[derive(Debug, Clone)]
pub struct Then<A, B> {
    first: A,
    second: B,
}

impl<A, B> Then<A, B> {
    pub fn new(first: A, second: B) -> Self {
        Self { first, second }
    }

    pub fn first(&self) -> &A {
        &self.first
    }

    pub fn second(&self) -> &B {
        &self.second
    }
}

/// Which side of a [`Then`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThenError<E1, E2> {
    First(E1),
    Second(E2),
}

impl<E1: fmt::Display, E2: fmt::Display> fmt::Display for ThenError<E1, E2> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::First(e) => write!(f, "first operator: {e}"),
            Self::Second(e) => write!(f, "second operator: {e}"),
        }
    }
}

impl<E1: StdError, E2: StdError> StdError for ThenError<E1, E2> {}

impl<A, B> Operator for Then<A, B>
where
    A: Operator + Clone,
    B: Operator<In = A::Out>,
{
    type In = A::In;
    type Out = B::Out;
    type Error = ThenError<A::Error, B::Error>;

    fn step(&mut self, change: ZSet<A::In>) -> Result<ZSet<B::Out>, Self::Error> {
        if change.is_empty() {
            return Ok(ZSet::new());
        }
        let mut first = self.first.clone();
        let mid = first.step(change).map_err(ThenError::First)?;
        if mid.is_empty() {
            self.first = first;
            return Ok(ZSet::new());
        }
        let out = self.second.step(mid).map_err(ThenError::Second)?;
        self.first = first;
        Ok(out)
    }
}

/// Stateless map, so a `ZSet::map` can sit in a pipeline.
#[derive(Debug, Clone)]
pub struct Map<I, O, F> {
    f: F,
    _io: PhantomData<fn(&I) -> O>,
}

impl<I, O, F> Map<I, O, F> {
    pub fn new(f: F) -> Self {
        Self {
            f,
            _io: PhantomData,
        }
    }
}

impl<I, O, F> Operator for Map<I, O, F>
where
    I: Ord,
    O: Ord,
    F: Fn(&I) -> O,
{
    type In = I;
    type Out = O;
    type Error = Infallible;

    fn step(&mut self, change: ZSet<I>) -> Result<ZSet<O>, Infallible> {
        Ok(change.map(&self.f))
    }
}

/// Stateless filter, so a `ZSet::filter` can sit in a pipeline.
#[derive(Debug, Clone)]
pub struct Filter<I, F> {
    keep: F,
    _item: PhantomData<fn(&I)>,
}

impl<I, F> Filter<I, F> {
    pub fn new(keep: F) -> Self {
        Self {
            keep,
            _item: PhantomData,
        }
    }
}

impl<I, F> Operator for Filter<I, F>
where
    I: Clone + Ord,
    F: Fn(&I) -> bool,
{
    type In = I;
    type Out = I;
    type Error = Infallible;

    fn step(&mut self, change: ZSet<I>) -> Result<ZSet<I>, Infallible> {
        Ok(change.filter(&self.keep))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::Sum;
    use crate::distinct::Distinct;
    use crate::zset::ZSet;

    fn rows<T: Ord>(rows: impl IntoIterator<Item = (T, i64)>) -> ZSet<T> {
        rows.into_iter().collect()
    }

    #[test]
    fn an_empty_change_produces_an_empty_change_and_touches_no_state() {
        let mut pipe = Distinct::new().then(Distinct::new());
        let _ = pipe.step(rows([("Wien", 1)])).unwrap();
        assert!(pipe.first().remembers(&"Wien"));
        assert!(pipe.second().remembers(&"Wien"));

        let out = pipe.step(ZSet::new()).unwrap();
        assert!(out.is_empty());
        assert!(pipe.first().remembers(&"Wien"));
        assert!(pipe.second().remembers(&"Wien"));
    }

    #[test]
    fn an_empty_output_from_the_first_operator_does_not_feed_the_second() {
        let mut pipe = Distinct::new().then(Distinct::new());
        let _ = pipe.step(rows([("Wien", 3)])).unwrap();
        let out = pipe.step(rows([("Wien", -1)])).unwrap();
        assert!(out.is_empty());
        assert!(pipe.first().remembers(&"Wien"));
        // Second still has the +1 from the first step, not a later empty.
        assert!(pipe.second().remembers(&"Wien"));
    }

    #[test]
    fn a_downstream_error_leaves_upstream_state_unchanged() {
        let mut pipe = Distinct::new().then(Sum::<u8, i64>::new());
        let change = rows([((0, i64::MAX), 1), ((0, 1), 1)]);
        assert_eq!(
            pipe.step(change.clone()),
            Err(ThenError::Second(crate::aggregate::Error::Overflow))
        );
        assert!(!pipe.first().remembers(&(0, i64::MAX)));
        assert!(!pipe.second().remembers(&0));
        assert_eq!(
            pipe.step(change),
            Err(ThenError::Second(crate::aggregate::Error::Overflow))
        );
    }
}
