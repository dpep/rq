//! Language-agnostic core: the common symbol model, repository identity, and
//! the wall clock every stored timestamp is measured against.
//!
//! Nothing here knows about a specific language. Language plugins (`crate::lang`)
//! emit [`Symbol`]s; everything else in rq operates on that shape.

mod clock;
mod identity;
mod symbol;

pub(crate) use clock::now_unix;
pub(crate) use identity::RepoIdentity;
pub(crate) use symbol::{Kind, Symbol};
