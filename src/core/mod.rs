//! Language-agnostic core: the common symbol model and repository identity.
//!
//! Nothing here knows about a specific language. Language plugins (`crate::lang`)
//! emit [`Symbol`]s; everything else in rq operates on that shape.

mod identity;
mod symbol;

pub use identity::RepoIdentity;
pub use symbol::{Kind, Symbol};
