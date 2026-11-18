//! Miroir turns a product's container image into Harbor environments graded from what the
//! agent did to the product: the rows it changed, the emails and requests the product sent, and
//! the answer it gave. Every module is public, so `main.rs` stays a thin command line over it.

pub mod bake;
pub mod check;
pub mod collect;
pub mod draft;
pub mod evidence;
pub mod fold;
pub mod grade;
pub mod journal;
pub mod listen;
pub mod manifest;
pub mod paths;
pub mod pg;
pub mod run;
pub mod sinks;
pub mod spec;
pub mod supervisor;
