//! History series share their records until a caller changes the series.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// A history record collection whose clones share an immutable backing vector.
///
/// Metadata in `HistoryData` remains independently owned. Mutating records
/// preserves the usual `Vec` clone semantics by copying only the changed series.
#[derive(Debug)]
pub struct SharedHistorySeries<T>(Arc<Vec<T>>);

impl<T> Clone for SharedHistorySeries<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Default for SharedHistorySeries<T> {
    fn default() -> Self {
        Self(Arc::new(Vec::new()))
    }
}

impl<T> From<Vec<T>> for SharedHistorySeries<T> {
    fn from(records: Vec<T>) -> Self {
        Self(Arc::new(records))
    }
}

impl<T> FromIterator<T> for SharedHistorySeries<T> {
    fn from_iter<I: IntoIterator<Item = T>>(records: I) -> Self {
        Self::from(records.into_iter().collect::<Vec<_>>())
    }
}

impl<T> Deref for SharedHistorySeries<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Clone> DerefMut for SharedHistorySeries<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl<T: PartialEq> PartialEq for SharedHistorySeries<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq> PartialEq<Vec<T>> for SharedHistorySeries<T> {
    fn eq(&self, other: &Vec<T>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq> PartialEq<SharedHistorySeries<T>> for Vec<T> {
    fn eq(&self, other: &SharedHistorySeries<T>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for SharedHistorySeries<T> {}

impl<T: Clone> IntoIterator for SharedHistorySeries<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        Arc::try_unwrap(self.0)
            .unwrap_or_else(|shared| (*shared).clone())
            .into_iter()
    }
}

impl<'a, T> IntoIterator for &'a SharedHistorySeries<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut SharedHistorySeries<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}
