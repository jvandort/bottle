//! The JSON bottle-cli takes and gives, as Rust types: what a program that runs bottle-cli (folio)
//! builds and reads, and what bottle-cli itself parses and prints, from one definition. Types only:
//! what bottle does with them is bottle's own (bottle-agent-client's policy.rs decides by a
//! Policy, bottle-disk builds a Recipe).
//!
//! What's empty (an empty list, no value) is left out of what's written: it reads back the same.
//! What it takes is refused with a key it doesn't know: a misspelt one is an error, not a setting
//! silently missing. A machine keeps its policy, read again by a later bottle-cli: a change to
//! what one may have comes with a conversion of those kept.

pub mod listing;
pub mod policy;
pub mod recipe;
