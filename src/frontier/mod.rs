//! Persistent URL frontier: normalization, storage, robots.txt and politeness.

pub mod normalizer;
pub mod robots;
pub mod scheduler;
pub mod store;

pub use normalizer::{
    domain_permitted, is_crawlable, normalize, normalize_absolute, priority_for_depth,
    registrable_domain,
};
pub use robots::{parse_robots, RobotsCache, RobotsRules};
pub use scheduler::DomainScheduler;
pub use store::{EnqueueOutcome, Store, UrlEntry};
