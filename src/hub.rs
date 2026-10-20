//! Publish/subscribe rooms
//!
//! A [`Hub`] fans messages out to everyone subscribed to a named room: chat
//! rooms, live dashboards, per-document editing sessions. Subscribers that fall
//! behind do not slow the others down: they are told how many messages they
//! missed and carry on with the newest ones.
//!
//! ```
//! # tokio_test_block(async {
//! use ullun::hub::{Delivery, Hub};
//!
//! let hub: Hub<String> = Hub::new(64);
//! let mut alice = hub.subscribe("lobby");
//! let mut bob = hub.subscribe("lobby");
//! assert_eq!(hub.publish("lobby", "hello".to_string()), 2);
//!
//! assert_eq!(alice.recv().await, Some(Delivery::Message("hello".to_string())));
//! assert_eq!(bob.recv().await, Some(Delivery::Message("hello".to_string())));
//! assert_eq!(hub.members("lobby"), 2);
//! drop(bob);
//! assert_eq!(hub.members("lobby"), 1);
//! # });
//! # fn tokio_test_block<F: std::future::Future>(f: F) {
//! #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f);
//! # }
//! ```
//!
//! Rooms exist while they have subscribers and disappear with the last one, so
//! a hub cannot be filled with empty rooms. Messages are not stored: keep
//! history yourself if late joiners need it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::broadcast;

struct Room<T> {
    tx: broadcast::Sender<T>,
    subscribers: usize,
}

struct Shared<T> {
    rooms: Mutex<HashMap<String, Room<T>>>,
    capacity: usize,
}

/// Named rooms with fan-out; cheap to clone, all clones share the rooms
pub struct Hub<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Hub<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

/// What a subscription yields
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery<T> {
    Message(T),
    /// The subscriber was too slow and this many messages were dropped for it
    Missed(u64),
}

/// A member of a room; leaves it when dropped
pub struct Subscription<T> {
    rx: broadcast::Receiver<T>,
    room: String,
    hub: Hub<T>,
}

impl<T: Clone + Send + 'static> Hub<T> {
    /// `capacity`: messages buffered per room for slow subscribers (at least 1)
    pub fn new(capacity: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                rooms: Mutex::new(HashMap::new()),
                capacity: capacity.max(1),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Room<T>>> {
        self.shared.rooms.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Join `room`, creating it if needed. Only messages published after this call arrive.
    pub fn subscribe(&self, room: &str) -> Subscription<T> {
        let mut rooms = self.lock();
        let entry = rooms.entry(room.to_string()).or_insert_with(|| Room {
            tx: broadcast::channel(self.shared.capacity).0,
            subscribers: 0,
        });
        entry.subscribers += 1;
        Subscription {
            rx: entry.tx.subscribe(),
            room: room.to_string(),
            hub: self.clone(),
        }
    }

    /// Send to everyone currently in `room`; returns how many received it
    /// (0 for a room nobody is in)
    pub fn publish(&self, room: &str, message: T) -> usize {
        let rooms = self.lock();
        rooms
            .get(room)
            .map(|r| r.tx.send(message).unwrap_or(0))
            .unwrap_or(0)
    }

    /// Subscribers in `room`
    pub fn members(&self, room: &str) -> usize {
        self.lock().get(room).map_or(0, |r| r.subscribers)
    }

    /// Rooms with at least one member, with their member counts, by name
    pub fn rooms(&self) -> Vec<(String, usize)> {
        let mut rooms: Vec<_> = self
            .lock()
            .iter()
            .map(|(name, r)| (name.clone(), r.subscribers))
            .collect();
        rooms.sort();
        rooms
    }
}

impl<T: Clone + Send + 'static> Subscription<T> {
    /// The next message, or how many were missed. `None` only if the hub is gone.
    pub async fn recv(&mut self) -> Option<Delivery<T>> {
        match self.rx.recv().await {
            Ok(m) => Some(Delivery::Message(m)),
            Err(broadcast::error::RecvError::Lagged(n)) => Some(Delivery::Missed(n)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    pub fn room(&self) -> &str {
        &self.room
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        let mut rooms = self
            .hub
            .shared
            .rooms
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(r) = rooms.get_mut(&self.room) {
            r.subscribers = r.subscribers.saturating_sub(1);
            if r.subscribers == 0 {
                rooms.remove(&self.room);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rooms_are_isolated_and_counted() {
        let hub: Hub<u32> = Hub::new(8);
        let mut a1 = hub.subscribe("a");
        let mut a2 = hub.subscribe("a");
        let mut b = hub.subscribe("b");

        assert_eq!(hub.publish("a", 1), 2);
        assert_eq!(hub.publish("b", 2), 1);
        assert_eq!(hub.publish("nobody", 3), 0);

        assert_eq!(a1.recv().await, Some(Delivery::Message(1)));
        assert_eq!(a2.recv().await, Some(Delivery::Message(1)));
        assert_eq!(b.recv().await, Some(Delivery::Message(2)));
        assert_eq!(hub.rooms(), [("a".to_string(), 2), ("b".to_string(), 1)]);
        assert_eq!(a1.room(), "a");
    }

    #[tokio::test]
    async fn late_joiners_only_see_new_messages_and_empty_rooms_vanish() {
        let hub: Hub<&'static str> = Hub::new(8);
        let early = hub.subscribe("r");
        hub.publish("r", "before");
        let mut late = hub.subscribe("r");
        hub.publish("r", "after");
        assert_eq!(late.recv().await, Some(Delivery::Message("after")));

        drop(early);
        assert_eq!(hub.members("r"), 1);
        drop(late);
        assert_eq!(hub.members("r"), 0);
        assert!(
            hub.rooms().is_empty(),
            "the last member takes the room with it"
        );
        assert_eq!(hub.publish("r", "void"), 0);
    }

    #[tokio::test]
    async fn slow_subscribers_learn_what_they_missed_and_do_not_block_others() {
        let hub: Hub<u32> = Hub::new(2);
        let mut slow = hub.subscribe("r");
        let mut fast = hub.subscribe("r");
        for i in 1..=5 {
            hub.publish("r", i);
            // the fast one keeps up
            assert_eq!(fast.recv().await, Some(Delivery::Message(i)));
        }
        // the slow one lost the oldest messages, then gets the newest two
        assert_eq!(slow.recv().await, Some(Delivery::Missed(3)));
        assert_eq!(slow.recv().await, Some(Delivery::Message(4)));
        assert_eq!(slow.recv().await, Some(Delivery::Message(5)));
    }

    #[tokio::test]
    async fn clones_share_rooms_and_zero_capacity_is_bumped() {
        let hub: Hub<u8> = Hub::new(0);
        let other = hub.clone();
        let mut sub = other.subscribe("x");
        assert_eq!(hub.publish("x", 9), 1);
        assert_eq!(sub.recv().await, Some(Delivery::Message(9)));
    }
}
