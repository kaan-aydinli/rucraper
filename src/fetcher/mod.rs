//! Safe, streaming HTTP fetching with retries and size limits.

pub mod http;

pub use http::{FetchError, FetchFailureKind, FetchedPage, Fetcher};
