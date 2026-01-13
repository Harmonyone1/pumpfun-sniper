//! Backpressure handling for high-throughput event processing
//!
//! ShredStream can emit bursts of data during high activity.
//! This module provides bounded channels with configurable drop policies
//! to prevent memory exhaustion and latency spikes.
//!
//! Trade-off: Dropping "oldest non-priority" under extreme load can
//! discard potentially profitable early token events. This is acceptable
//! because:
//! 1. Tracked wallet events (highest value) are preserved
//! 2. Under burst conditions, most dropped events are noise
//! 3. Latency from unbounded queues is worse than occasional drops

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::DropPolicy as ConfigDropPolicy;

/// Drop policy for when queue is full
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DropPolicy {
    /// Drop oldest non-priority items first
    OldestNonPriority,
    /// Drop newest incoming items
    Newest,
    /// Block until space available (not recommended for real-time)
    Block,
}

impl From<ConfigDropPolicy> for DropPolicy {
    fn from(policy: ConfigDropPolicy) -> Self {
        match policy {
            ConfigDropPolicy::OldestNonPriority => DropPolicy::OldestNonPriority,
            ConfigDropPolicy::Newest => DropPolicy::Newest,
            ConfigDropPolicy::Block => DropPolicy::Block,
        }
    }
}

/// Event with priority flag
#[derive(Debug, Clone)]
pub struct PrioritizedEvent<T> {
    pub event: T,
    pub is_priority: bool,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl<T> PrioritizedEvent<T> {
    pub fn new(event: T, is_priority: bool) -> Self {
        Self {
            event,
            is_priority,
            timestamp: chrono::Utc::now(),
        }
    }
}

/// Bounded channel with backpressure handling
pub struct BackpressureChannel<T> {
    capacity: usize,
    drop_policy: DropPolicy,
    buffer: Arc<Mutex<VecDeque<PrioritizedEvent<T>>>>,
    tx: mpsc::Sender<()>,
    rx: Arc<Mutex<mpsc::Receiver<()>>>,
    dropped_count: Arc<Mutex<u64>>,
}

impl<T: Clone + Send + 'static> BackpressureChannel<T> {
    /// Create a new backpressure channel
    pub fn new(capacity: usize, drop_policy: DropPolicy) -> Self {
        let (tx, rx) = mpsc::channel(capacity);

        Self {
            capacity,
            drop_policy,
            buffer: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            tx,
            rx: Arc::new(Mutex::new(rx)),
            dropped_count: Arc::new(Mutex::new(0)),
        }
    }

    /// Send an event to the channel
    pub async fn send(&self, event: T, is_priority: bool) -> Result<(), String> {
        let prioritized = PrioritizedEvent::new(event, is_priority);

        let mut buffer = self
            .buffer
            .lock()
            .map_err(|e| format!("Buffer lock failed: {}", e))?;

        if buffer.len() >= self.capacity {
            match self.drop_policy {
                DropPolicy::OldestNonPriority => {
                    // Find and remove oldest non-priority item
                    if let Some(idx) = buffer.iter().position(|e| !e.is_priority) {
                        buffer.remove(idx);
                        *self.dropped_count.lock().unwrap() += 1;
                        debug!("Dropped oldest non-priority event due to backpressure");
                    } else if !is_priority {
                        // All items are priority, drop incoming non-priority
                        *self.dropped_count.lock().unwrap() += 1;
                        debug!("Dropped incoming non-priority event (all queued are priority)");
                        return Ok(());
                    } else {
                        // Incoming is priority, drop oldest priority
                        buffer.pop_front();
                        *self.dropped_count.lock().unwrap() += 1;
                        debug!("Dropped oldest priority event to make room for new priority");
                    }
                }
                DropPolicy::Newest => {
                    // Drop the incoming event
                    *self.dropped_count.lock().unwrap() += 1;
                    debug!("Dropped newest event due to backpressure");
                    return Ok(());
                }
                DropPolicy::Block => {
                    // Wait for space (dangerous for real-time)
                    warn!("Backpressure channel is full, blocking...");
                    drop(buffer); // Release lock while waiting

                    // Wait for notification that space is available
                    // This is a simplified blocking behavior
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

                    // Retry
                    return Box::pin(self.send(prioritized.event, is_priority)).await;
                }
            }
        }

        buffer.push_back(prioritized);

        // Notify receiver
        let _ = self.tx.try_send(());

        Ok(())
    }

    /// Receive an event from the channel
    pub async fn recv(&self) -> Option<PrioritizedEvent<T>> {
        // Wait for notification
        {
            let mut rx = self.rx.lock().ok()?;
            rx.recv().await?;
        }

        // Get item from buffer
        let mut buffer = self.buffer.lock().ok()?;
        buffer.pop_front()
    }

    /// Try to receive without blocking
    pub fn try_recv(&self) -> Option<PrioritizedEvent<T>> {
        let mut buffer = self.buffer.lock().ok()?;
        buffer.pop_front()
    }

    /// Get current buffer size
    pub fn len(&self) -> usize {
        self.buffer.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get number of dropped events
    pub fn dropped_count(&self) -> u64 {
        *self.dropped_count.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get capacity
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get utilization percentage
    pub fn utilization(&self) -> f64 {
        (self.len() as f64 / self.capacity as f64) * 100.0
    }
}

/// Priority queue for processing high-value events first
pub struct PriorityQueue<T> {
    priority: VecDeque<T>,
    normal: VecDeque<T>,
    capacity: usize,
}

impl<T> PriorityQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            priority: VecDeque::with_capacity(capacity / 2),
            normal: VecDeque::with_capacity(capacity / 2),
            capacity,
        }
    }

    pub fn push(&mut self, item: T, is_priority: bool) {
        if is_priority {
            self.priority.push_back(item);
        } else {
            self.normal.push_back(item);
        }

        // Enforce capacity by dropping normal items first
        while self.len() > self.capacity && !self.normal.is_empty() {
            self.normal.pop_front();
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        // Always process priority items first
        self.priority
            .pop_front()
            .or_else(|| self.normal.pop_front())
    }

    pub fn len(&self) -> usize {
        self.priority.len() + self.normal.len()
    }

    pub fn is_empty(&self) -> bool {
        self.priority.is_empty() && self.normal.is_empty()
    }

    pub fn priority_len(&self) -> usize {
        self.priority.len()
    }

    pub fn normal_len(&self) -> usize {
        self.normal.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_backpressure_oldest_non_priority() {
        let channel: BackpressureChannel<i32> =
            BackpressureChannel::new(3, DropPolicy::OldestNonPriority);

        // Fill channel with non-priority items
        channel.send(1, false).await.unwrap();
        channel.send(2, false).await.unwrap();
        channel.send(3, false).await.unwrap();

        assert_eq!(channel.len(), 3);

        // Add priority item, should drop oldest non-priority
        channel.send(4, true).await.unwrap();

        assert_eq!(channel.len(), 3);
        assert_eq!(channel.dropped_count(), 1);
    }

    #[tokio::test]
    async fn test_backpressure_newest() {
        let channel: BackpressureChannel<i32> = BackpressureChannel::new(2, DropPolicy::Newest);

        channel.send(1, false).await.unwrap();
        channel.send(2, false).await.unwrap();
        channel.send(3, false).await.unwrap(); // Should be dropped

        assert_eq!(channel.len(), 2);
        assert_eq!(channel.dropped_count(), 1);
    }

    #[test]
    fn test_priority_queue() {
        let mut queue = PriorityQueue::new(10);

        queue.push("normal1", false);
        queue.push("priority1", true);
        queue.push("normal2", false);
        queue.push("priority2", true);

        // Should get priority items first
        assert_eq!(queue.pop(), Some("priority1"));
        assert_eq!(queue.pop(), Some("priority2"));
        assert_eq!(queue.pop(), Some("normal1"));
        assert_eq!(queue.pop(), Some("normal2"));
    }
}
