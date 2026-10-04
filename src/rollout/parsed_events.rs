//! Parsed event prefixes remain shared when a history scan or tail parser forks.

use std::fmt;
use std::iter::FusedIterator;
use std::sync::Arc;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::ParsedEvent;

// Ordinary append copies at most the final incomplete chunk, never the whole
// historical event buffer. A foreign baseline update copies only its target
// chunk, which can be a different, already complete chunk.
const CHUNK_EVENTS: usize = 256;

#[derive(Clone, Debug, Default)]
pub(super) struct ParsedEvents {
    chunks: Vec<Arc<Vec<ParsedEvent>>>,
    len: usize,
}

impl ParsedEvents {
    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn push(&mut self, event: ParsedEvent) {
        if self.len.is_multiple_of(CHUNK_EVENTS) {
            self.chunks.push(Arc::new(Vec::new()));
        }
        let chunk = Arc::make_mut(self.chunks.last_mut().expect("push creates its chunk"));
        if chunk.len() == chunk.capacity() {
            // Vec::clone used by COW has capacity=len. Its ordinary growth at
            // len=255 would reserve 510 entries, so explicitly cap growth here.
            // Small rollouts also avoid reserving a full chunk for a few events.
            let capacity = chunk.capacity().max(2).saturating_mul(2).min(CHUNK_EVENTS);
            chunk.reserve_exact(capacity - chunk.len());
        }
        chunk.push(event);
        self.len += 1;
    }

    pub(super) fn get_mut(&mut self, index: usize) -> Option<&mut ParsedEvent> {
        if index >= self.len {
            return None;
        }
        Arc::make_mut(&mut self.chunks[index / CHUNK_EVENTS]).get_mut(index % CHUNK_EVENTS)
    }

    pub(super) fn iter(&self) -> ParsedEventsIter<'_> {
        ParsedEventsIter {
            events: self,
            front: 0,
            back: self.len,
        }
    }

    pub(super) fn starts_with(&self, prefix: &Self) -> bool {
        self.len >= prefix.len
            && self
                .iter()
                .zip(prefix.iter())
                .all(|(left, right)| left == right)
    }
}

impl PartialEq for ParsedEvents {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

pub(super) struct ParsedEventsIter<'a> {
    events: &'a ParsedEvents,
    front: usize,
    back: usize,
}

impl<'a> Iterator for ParsedEventsIter<'a> {
    type Item = &'a ParsedEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let index = self.front;
        self.front += 1;
        Some(&self.events.chunks[index / CHUNK_EVENTS][index % CHUNK_EVENTS])
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back - self.front;
        (remaining, Some(remaining))
    }
}

impl DoubleEndedIterator for ParsedEventsIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(&self.events.chunks[self.back / CHUNK_EVENTS][self.back % CHUNK_EVENTS])
    }
}

impl ExactSizeIterator for ParsedEventsIter<'_> {}
impl FusedIterator for ParsedEventsIter<'_> {}

impl<'a> IntoIterator for &'a ParsedEvents {
    type Item = &'a ParsedEvent;
    type IntoIter = ParsedEventsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Serialize for ParsedEvents {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for ParsedEvents {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EventsVisitor;

        impl<'de> Visitor<'de> for EventsVisitor {
            type Value = ParsedEvents;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a flat sequence of parsed rollout events")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut events = ParsedEvents::default();
                while let Some(event) = sequence.next_element()? {
                    events.push(event);
                }
                Ok(events)
            }
        }

        // Decode directly into chunks instead of retaining a temporary full Vec
        // alongside the chunk storage during persistent-cache hydration.
        deserializer.deserialize_seq(EventsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    #[test]
    fn rollout_memory_shared_partial_chunk_append_caps_allocation_and_preserves_prefix() {
        let timestamp = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap();
        let mut events = ParsedEvents::default();
        for _ in 0..(CHUNK_EVENTS * 2 - 1) {
            events.push(ParsedEvent::Activity { timestamp });
        }
        let shared = events.clone();
        events.push(ParsedEvent::Activity { timestamp });
        assert!(Arc::ptr_eq(&events.chunks[0], &shared.chunks[0]));
        assert!(!Arc::ptr_eq(&events.chunks[1], &shared.chunks[1]));
        assert_eq!(shared.chunks[1].len(), CHUNK_EVENTS - 1);
        assert_eq!(events.chunks[1].len(), CHUNK_EVENTS);
        assert_eq!(
            events.chunks[1].capacity(),
            CHUNK_EVENTS,
            "255-event COW followed by push must not allocate 510 events"
        );
        let full_prefix = events.clone();
        events.push(ParsedEvent::Activity { timestamp });
        assert!(Arc::ptr_eq(&events.chunks[0], &full_prefix.chunks[0]));
        assert!(Arc::ptr_eq(&events.chunks[1], &full_prefix.chunks[1]));
        assert_eq!(events.chunks[2].len(), 1);
        assert!(
            events
                .chunks
                .iter()
                .all(|chunk| chunk.capacity() <= CHUNK_EVENTS)
        );
    }
}
