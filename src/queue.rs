use crate::{TelemetryError, TelemetryResult};
use alloc::collections::VecDeque;

/// Items stored in the queue must report (approximately) how much memory they
/// account for.
pub trait ByteCost {
    /// Approximate heap+payload memory attributable to this queued item.
    fn byte_cost(&self) -> usize;
}

/// Convert float multiplier to ratio (num, den).
#[inline]
fn float_to_ratio(mult: f64) -> (usize, usize) {
    // Clamp to sane range
    let mult = mult.clamp(1.01, 16.0);

    const DEN: usize = 1024;

    // Convert using truncation + guarantee progress
    let num = (mult * DEN as f64) as usize;

    // Ensure strictly > 1.0 growth
    let num = num.max(DEN + 1);

    (num, DEN)
}

/// A double-ended queue with a maximum byte budget.
///
/// Policy:
/// - Byte budget is enforced by evicting from the front until the new item fits.
/// - Element capacity is a **hard cap**: the underlying `VecDeque` is pre-allocated
///   to `max_elems` and we never call `reserve*`, so it will not grow.
/// - When the ring is full, we evict one item from the front before pushing.
/// - `cur_bytes` is kept consistent for *all* removal paths.
#[derive(Debug, Clone)]
pub struct BoundedDeque<T> {
    q: VecDeque<T>,
    max_bytes: usize,
    cur_bytes: usize,
    max_elems: usize,
    grow_num: usize,
    grow_den: usize,
}

impl<T: ByteCost> BoundedDeque<T> {
    fn prepare_push_fifo(&mut self, cost: usize) -> TelemetryResult<()> {
        if cost > self.max_bytes {
            return Err(TelemetryError::PacketTooLarge(
                "Item exceeds maximum byte budget",
            ));
        }

        while !self.q.is_empty() && self.cur_bytes + cost > self.max_bytes {
            let _ = self.pop_front();
        }

        if self.q.len() >= self.max_elems {
            let _ = self.pop_front();
        }

        self.ensure_room_for_one();
        Ok(())
    }

    /// Create new bounded deque with byte budget and element cap.
    ///
    /// `starting_elems` controls the initial allocation but will be clamped
    /// to `max_elems`.
    ///
    /// Notes:
    /// - `max_elems` is derived conservatively from `size_of::<T>()` because
    ///   `ByteCost` is dynamic. This prevents runaway element counts even if
    ///   `byte_cost()` is small.
    pub fn new(max_bytes: usize, starting_bytes: usize, grow_mult: f64) -> Self {
        if starting_bytes > max_bytes {
            panic!(
                "starting_bytes ({}) must be less than max_bytes ({}) to avoid conflicts",
                starting_bytes, max_bytes
            );
        }
        if max_bytes == 0 {
            panic!("max_bytes must be greater than 0");
        }
        if grow_mult <= 1.0 {
            panic!("grow_mult must be greater than 1.0");
        }
        let min_cost = size_of::<T>().max(1);
        let max_elems = (max_bytes / min_cost).max(1);
        let starting_elems = starting_bytes / min_cost;
        let start_cap = starting_elems.clamp(1, max_elems);

        let (grow_num, grow_den) = float_to_ratio(grow_mult);

        Self {
            q: VecDeque::with_capacity(start_cap),
            max_bytes,
            cur_bytes: 0,
            max_elems,
            grow_num,
            grow_den,
        }
    }

    /// Current length.
    #[inline]
    pub fn len(&self) -> usize {
        self.q.len()
    }

    /// Check if empty.
    #[allow(dead_code)]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// Get iterator over items.
    #[allow(dead_code)]
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item=&T> {
        self.q.iter()
    }

    /// Check if item is contained in the queue.
    #[inline]
    pub fn contains(&self, v: &T) -> bool
    where
        T: PartialEq,
    {
        self.q.contains(v)
    }

    /// Clear all items.
    #[inline]
    pub fn clear(&mut self) {
        self.q.clear();
        self.cur_bytes = 0;
    }

    /// Current used bytes (according to `ByteCost`).
    #[allow(dead_code)]
    #[inline]
    pub fn bytes_used(&self) -> usize {
        self.cur_bytes
    }

    /// Maximum allowed bytes.
    #[allow(dead_code)]
    #[inline]
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Maximum allowed elements (hard cap).
    #[allow(dead_code)]
    #[inline]
    pub fn max_elems(&self) -> usize {
        self.max_elems
    }

    /// Underlying `VecDeque` capacity (should stay == `max_elems`).
    #[allow(dead_code)]
    #[inline]
    pub fn capacity(&self) -> usize {
        self.q.capacity()
    }

    /// Pop from front, updating byte count.
    pub fn pop_front(&mut self) -> Option<T> {
        let v = self.q.pop_front()?;
        self.cur_bytes = self.cur_bytes.saturating_sub(v.byte_cost());
        Some(v)
    }

    /// Pop from back, updating byte count.
    #[allow(dead_code)]
    pub fn pop_back(&mut self) -> Option<T> {
        let v = self.q.pop_back()?;
        self.cur_bytes = self.cur_bytes.saturating_sub(v.byte_cost());
        Some(v)
    }

    /// Remove item at position, updating byte count.
    pub fn remove_pos(&mut self, idx: usize) -> Option<T> {
        let v = self.q.remove(idx)?;
        self.cur_bytes = self.cur_bytes.saturating_sub(v.byte_cost());
        Some(v)
    }

    /// Remove first occurrence of value, updating byte count.
    pub fn remove_value(&mut self, needle: &T)
    where
        T: PartialEq,
    {
        if let Some(idx) = self.q.iter().position(|x| x == needle) {
            let _ = self.remove_pos(idx);
        }
    }

    /// Retain only the elements specified by the predicate, updating byte count.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&T) -> bool,
    {
        let mut idx = 0;
        while idx < self.q.len() {
            let keep_item = self.q.get(idx).is_some_and(&mut keep);
            if keep_item {
                idx += 1;
            } else {
                let _ = self.remove_pos(idx);
            }
        }
    }

    /// Ensure there is room for one more element *without* `push_back` triggering growth.
    ///
    /// Multiplicative growth: new_cap = ceil(cap * grow_num / grow_den), capped at max_elems.
    /// Always guarantees progress by forcing target_cap >= cap + 1 when growing.
    fn ensure_room_for_one(&mut self) {
        let len = self.q.len();
        let cap = self.q.capacity();

        if len < cap {
            return;
        }

        // Hard length cap: ring eviction.
        if len >= self.max_elems {
            let _ = self.pop_front();
            return;
        }

        // If we've reached the cap (or allocator rounded capacity above it), don't grow.
        if cap >= self.max_elems {
            let _ = self.pop_front();
            return;
        }

        // ---- multiplicative growth ----
        // Example: 2x => grow_num=2, grow_den=1
        // Example: 1.5x => grow_num=3, grow_den=2
        let grow_num: usize = self.grow_num; // must be >= 1
        let grow_den: usize = self.grow_den; // must be >= 1

        // ceil(cap * grow_num / grow_den) without floats:
        // (cap*grow_num + grow_den - 1) / grow_den
        let scaled = cap.saturating_mul(grow_num).saturating_add(grow_den - 1);
        let mut target_cap = scaled / grow_den;

        // Ensure we actually grow (avoid target_cap == cap).
        target_cap = target_cap.max(cap + 1);

        // Cap growth at max_elems.
        target_cap = target_cap.min(self.max_elems);

        // Reserve exactly the delta from current capacity to requested capacity.
        let add = target_cap.saturating_sub(cap);
        if add > 0 {
            self.q.reserve_exact(add);
        } else {
            // Shouldn't happen due to max(cap+1), but keep it safe.
            let _ = self.pop_front();
        }

        debug_assert!(self.q.len() < self.q.capacity());
    }

    /// Push to back, evicting from front as needed to stay within byte budget.
    ///
    /// Guarantees:
    /// - Never grows allocation beyond `max_elems` (no reserve calls; ring eviction).
    /// - Maintains `cur_bytes` consistency.
    pub fn push_back(&mut self, v: T) -> TelemetryResult<()> {
        let cost = v.byte_cost();
        self.prepare_push_fifo(cost)?;

        // At this point, push cannot require a reallocation because:
        // - capacity was pre-allocated to max_elems
        // - len < max_elems (or we just evicted)
        self.q.push_back(v);
        self.cur_bytes = self.cur_bytes.saturating_add(cost);
        Ok(())
    }

    /// Push while preserving descending priority order.
    ///
    /// `priority_of` must return a larger value for higher-priority items.
    /// Items with equal priority preserve FIFO order.
    pub fn push_back_prioritized<F>(&mut self, v: T, mut priority_of: F) -> TelemetryResult<()>
    where
        F: FnMut(&T) -> u8,
    {
        let cost = v.byte_cost();
        if cost > self.max_bytes {
            return Err(TelemetryError::PacketTooLarge(
                "Item exceeds maximum byte budget",
            ));
        }

        let new_priority = priority_of(&v);
        while !self.q.is_empty() && self.cur_bytes + cost > self.max_bytes {
            let tail_priority = self.q.back().map(&mut priority_of).unwrap_or(0);
            if tail_priority > new_priority {
                return Err(TelemetryError::Io("priority queue saturated"));
            }
            let _ = self.pop_back();
        }

        if self.q.len() >= self.max_elems {
            let tail_priority = self.q.back().map(&mut priority_of).unwrap_or(0);
            if tail_priority > new_priority {
                return Err(TelemetryError::Io("priority queue saturated"));
            }
            let _ = self.pop_back();
        }

        self.ensure_room_for_one();

        let insert_at = self
            .q
            .iter()
            .position(|existing| priority_of(existing) < new_priority);
        if let Some(idx) = insert_at {
            self.q.insert(idx, v);
        } else {
            self.q.push_back(v);
        }
        self.cur_bytes = self.cur_bytes.saturating_add(cost);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Item {
        id: u8,
        cost: usize,
        priority: u8,
    }

    impl ByteCost for Item {
        fn byte_cost(&self) -> usize {
            self.cost
        }
    }

    #[test]
    fn prioritized_queue_preserves_fifo_within_same_priority() {
        let mut q = BoundedDeque::new(64, 16, 2.0);
        q.push_back_prioritized(
            Item {
                id: 1,
                cost: 1,
                priority: 10,
            },
            |item| item.priority,
        )
            .unwrap();
        q.push_back_prioritized(
            Item {
                id: 2,
                cost: 1,
                priority: 10,
            },
            |item| item.priority,
        )
            .unwrap();
        q.push_back_prioritized(
            Item {
                id: 3,
                cost: 1,
                priority: 20,
            },
            |item| item.priority,
        )
            .unwrap();

        assert_eq!(q.pop_front().unwrap().id, 3);
        assert_eq!(q.pop_front().unwrap().id, 1);
        assert_eq!(q.pop_front().unwrap().id, 2);
    }

    #[test]
    fn prioritized_queue_drops_lower_priority_when_saturated() {
        let mut q = BoundedDeque::new(64, 32, 2.0);
        q.push_back_prioritized(
            Item {
                id: 1,
                cost: 32,
                priority: 20,
            },
            |item| item.priority,
        )
            .unwrap();
        q.push_back_prioritized(
            Item {
                id: 2,
                cost: 32,
                priority: 20,
            },
            |item| item.priority,
        )
            .unwrap();

        let err = q
            .push_back_prioritized(
                Item {
                    id: 3,
                    cost: 32,
                    priority: 10,
                },
                |item| item.priority,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            TelemetryError::Io("priority queue saturated")
        ));
        assert_eq!(q.pop_front().unwrap().id, 1);
        assert_eq!(q.pop_front().unwrap().id, 2);
    }
}
