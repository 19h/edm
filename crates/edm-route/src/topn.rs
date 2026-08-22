//! A bounded heap over a total order.
//!
//! Two jobs. It keeps the best `capacity` items seen, and its [`TopN::worst`]
//! is the branch-and-bound threshold: it stays `None` until the heap fills, so
//! the bounds are inert during warm-up and tighten as good candidates arrive.
//!
//! The key is required to be `Ord` rather than merely `PartialOrd`, which is
//! the whole point. A partial order over rates would make "the maximum"
//! ill-defined under ties, and the ranking would then depend on the order the
//! instance happened to arrive in.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Keeps the `capacity` largest items offered to it.
#[derive(Debug)]
pub struct TopN<K: Ord, T> {
    capacity: usize,
    // A min-heap by key, so the item to evict is the one at the top.
    heap: BinaryHeap<Reverse<Entry<K, T>>>,
}

#[derive(Debug)]
struct Entry<K: Ord, T> {
    key: K,
    value: T,
}

impl<K: Ord, T> PartialEq for Entry<K, T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K: Ord, T> Eq for Entry<K, T> {}

impl<K: Ord, T> PartialOrd for Entry<K, T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: Ord, T> Ord for Entry<K, T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

impl<K: Ord, T> TopN<K, T> {
    /// An empty heap of the given capacity. A capacity of zero accepts nothing.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            heap: BinaryHeap::new(),
        }
    }

    /// How many items are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether nothing has been kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// The key a candidate must beat to be worth building, or `None` while the
    /// heap has room for anything at all.
    #[must_use]
    pub fn worst(&self) -> Option<&K> {
        if self.heap.len() < self.capacity {
            return None;
        }
        self.heap.peek().map(|Reverse(entry)| &entry.key)
    }

    /// Offers an item. Returns whether it was kept.
    pub fn offer(&mut self, key: K, value: T) -> bool {
        if self.capacity == 0 {
            return false;
        }
        if self.heap.len() < self.capacity {
            self.heap.push(Reverse(Entry { key, value }));
            return true;
        }
        let Some(Reverse(worst)) = self.heap.peek() else {
            return false;
        };
        if key <= worst.key {
            return false;
        }
        self.heap.pop();
        self.heap.push(Reverse(Entry { key, value }));
        true
    }

    /// Empties the heap, best first.
    pub fn drain(self) -> Vec<T> {
        let mut items: Vec<Entry<K, T>> =
            self.heap.into_iter().map(|Reverse(entry)| entry).collect();
        items.sort_by(|a, b| b.key.cmp(&a.key));
        items.into_iter().map(|entry| entry.value).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::TopN;

    #[test]
    fn keeps_the_largest_and_drains_best_first() {
        let mut heap = TopN::new(3);
        for i in [5, 1, 9, 3, 7, 2] {
            heap.offer(i, format!("v{i}"));
        }
        assert_eq!(heap.drain(), vec!["v9", "v7", "v5"]);
    }

    #[test]
    fn worst_is_none_until_full_then_is_the_threshold() {
        let mut heap = TopN::new(2);
        assert_eq!(heap.worst(), None);
        heap.offer(5, ());
        assert_eq!(heap.worst(), None);
        heap.offer(9, ());
        assert_eq!(heap.worst(), Some(&5));
        heap.offer(7, ());
        assert_eq!(heap.worst(), Some(&7));
    }

    #[test]
    fn a_tie_with_the_worst_does_not_evict_it() {
        // Ties matter: the ranking key ends in an absolute tie-break, so two
        // keys that compare equal are the same route and swapping them would
        // be churn without meaning.
        let mut heap = TopN::new(1);
        heap.offer(5, "first");
        assert!(!heap.offer(5, "second"));
        assert_eq!(heap.drain(), vec!["first"]);
    }

    #[test]
    fn a_zero_capacity_heap_keeps_nothing() {
        let mut heap = TopN::new(0);
        assert!(!heap.offer(1, ()));
        assert!(heap.is_empty());
        assert_eq!(heap.len(), 0);
    }

    #[test]
    fn a_shuffled_offer_order_produces_the_same_ranking() {
        let forward = {
            let mut heap = TopN::new(4);
            for i in 0..20 {
                heap.offer(i % 7, i);
            }
            heap.drain()
        };
        let backward = {
            let mut heap = TopN::new(4);
            for i in (0..20).rev() {
                heap.offer(i % 7, i);
            }
            heap.drain()
        };
        // The keys agree even though the payloads that carried them differ:
        // that is exactly what the total-order requirement buys, and it is why
        // a real ranking key ends in a tie-break derived from the route itself.
        let keys = |v: Vec<i32>| v.into_iter().map(|i| i % 7).collect::<Vec<_>>();
        assert_eq!(keys(forward), keys(backward));
    }
}
