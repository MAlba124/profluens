//! The one integration-test binary. Sub-suites are modules sharing one link and allocator.

// Tests build throwaway vectors of rows.
#![allow(clippy::disallowed_methods)]

mod alloc_count;
mod no_alloc;
mod property;
mod rejects;

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;
