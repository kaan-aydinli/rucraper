//! `pretrain-scraper` — a bounded-memory Rust web crawler and cleaning engine
//! that emits LLM-pretraining-ready JSONL shards.

pub mod config;
pub mod dedup;
pub mod error;
pub mod export;
pub mod extractor;
pub mod fetcher;
pub mod filter;
pub mod frontier;
pub mod pipeline;
pub mod storage;
pub mod telemetry;
