//! Per-domain politeness: one gate per domain, bounded concurrency per domain.
//!
//! Each domain owns an independent semaphore (limits parallel requests) and an
//! async gate storing the earliest instant at which the next request may start.
//! There is no global lock: domains never block each other, and a slow host only
//! parks its own tasks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{Result, ScraperError};

#[derive(Debug)]
struct DomainSlot {
    semaphore: Arc<Semaphore>,
    next_allowed: tokio::sync::Mutex<Instant>,
}

/// Scheduler enforcing per-domain concurrency and delay.
#[derive(Debug)]
pub struct DomainScheduler {
    slots: DashMap<String, Arc<DomainSlot>>,
    max_per_domain: usize,
    min_delay: Duration,
}

impl DomainScheduler {
    /// Create a scheduler with a per-domain concurrency cap and minimum delay.
    pub fn new(max_per_domain: usize, min_delay: Duration) -> Self {
        Self {
            slots: DashMap::new(),
            max_per_domain: max_per_domain.max(1),
            min_delay,
        }
    }

    /// Wait until a request to `domain` is polite, then return the permit that
    /// must be held for the duration of that request.
    pub async fn acquire(
        &self,
        domain: &str,
        crawl_delay: Option<Duration>,
    ) -> Result<OwnedSemaphorePermit> {
        let slot = self.slot(domain);
        let permit = slot
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ScraperError::Other("domain scheduler closed".to_string()))?;
        let effective = self.min_delay.max(crawl_delay.unwrap_or_default());
        // The gate mutex is deliberately held across the sleep so that requests
        // to one domain leave in a well-spaced order.
        let mut gate = slot.next_allowed.lock().await;
        let now = Instant::now();
        if *gate > now {
            tokio::time::sleep(*gate - now).await;
        }
        *gate = Instant::now() + effective;
        Ok(permit)
    }

    /// Number of domains currently tracked (diagnostics/tests).
    pub fn tracked_domains(&self) -> usize {
        self.slots.len()
    }

    fn slot(&self, domain: &str) -> Arc<DomainSlot> {
        if let Some(existing) = self.slots.get(domain) {
            return existing.clone();
        }
        let created = Arc::new(DomainSlot {
            semaphore: Arc::new(Semaphore::new(self.max_per_domain)),
            next_allowed: tokio::sync::Mutex::new(Instant::now()),
        });
        self.slots
            .entry(domain.to_string())
            .or_insert_with(|| created.clone())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serialises_requests_per_domain() {
        let scheduler = DomainScheduler::new(4, Duration::from_millis(40));
        let started = Instant::now();
        for _ in 0..3 {
            let permit = scheduler.acquire("example.com", None).await.unwrap();
            drop(permit);
        }
        // Two waits of ~40ms must have happened.
        assert!(
            started.elapsed() >= Duration::from_millis(70),
            "elapsed: {:?}",
            started.elapsed()
        );
        assert_eq!(scheduler.tracked_domains(), 1);
    }

    #[tokio::test]
    async fn honours_larger_crawl_delay() {
        let scheduler = DomainScheduler::new(2, Duration::from_millis(10));
        let _permit = scheduler
            .acquire("example.com", Some(Duration::from_millis(120)))
            .await
            .unwrap();
        let started = Instant::now();
        let _permit2 = scheduler.acquire("example.com", None).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(80));
    }

    #[tokio::test]
    async fn domains_do_not_block_each_other() {
        let scheduler = DomainScheduler::new(1, Duration::from_millis(200));
        let _first = scheduler.acquire("a.com", None).await.unwrap();
        let started = Instant::now();
        let _second = scheduler.acquire("b.com", None).await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(150));
    }
}
