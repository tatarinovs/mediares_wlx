//! Page selection and on-demand decoding; a print job never holds the whole TIFF in memory.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;

use crate::image_cache::{self, DecodeOptions, DecodedImage};
use mediares_core::probe::MediaType;

#[derive(Clone)]
pub(crate) struct Source {
    pub path: PathBuf,
    pub kind: MediaType,
    pub options: DecodeOptions,
    pub quarter_turns: u8,
}

impl Source {
    pub fn turn(&self, mut image: Arc<DecodedImage>) -> Arc<DecodedImage> {
        match self.quarter_turns % 4 {
            3 => Arc::new(image_cache::rotated(&image, false)),
            n => {
                for _ in 0..n {
                    image = Arc::new(image_cache::rotated(&image, true));
                }
                image
            }
        }
    }

    pub fn decode(&self, page: u32, cancelled: &dyn Fn() -> bool) -> Option<Arc<DecodedImage>> {
        image_cache::decode(
            &self.path,
            self.kind,
            DecodeOptions {
                page,
                fit: 0,
                ..self.options
            },
            cancelled,
        )
        .map(Arc::new)
        .map(|image| self.turn(image))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pages(Vec<RangeInclusive<u32>>);

impl Pages {
    pub fn all(count: u32) -> Self {
        Self(std::iter::once(0..=count.max(1) - 1).collect())
    }

    /// One-based input, zero-based output in document order, with overlaps merged.
    pub fn parse(text: &str, count: u32) -> Option<Self> {
        let mut ranges = Vec::new();
        for part in text.split([',', ';']) {
            let mut ends = part.trim().split(['-', '–', '—']);
            let first = ends.next()?.trim().parse::<u32>().ok()?;
            let last = match ends.next() {
                Some(last) => last.trim().parse::<u32>().ok()?,
                None => first,
            };
            if ends.next().is_some() || first == 0 || first > last || last > count {
                return None;
            }
            ranges.push((first - 1, last - 1));
        }
        ranges.sort_unstable();
        let mut merged: Vec<RangeInclusive<u32>> = Vec::new();
        for (first, last) in ranges {
            if let Some(previous) = merged.last_mut() {
                if first <= previous.end().saturating_add(1) {
                    *previous = *previous.start()..=last.max(*previous.end());
                    continue;
                }
            }
            merged.push(first..=last);
        }
        Some(Self(merged))
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().cloned().flatten()
    }

    pub fn len(&self) -> u64 {
        self.0
            .iter()
            .map(|r| u64::from(*r.end()) - u64::from(*r.start()) + 1)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_merges_ranges_and_keeps_document_order() {
        let pages = Pages::parse("5, 1-3; 2–4, 7—8, 8", 8).unwrap();
        assert_eq!(pages.iter().collect::<Vec<_>>(), [0, 1, 2, 3, 4, 6, 7]);
        assert_eq!(pages.len(), 7);
        assert_eq!(Pages::all(3).iter().collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn rejects_empty_reversed_out_of_bounds_and_malformed_selections() {
        for input in [
            "",
            "0",
            "4",
            "3-1",
            "1-4",
            "1,",
            ",1",
            "1--2",
            "-1",
            "1-",
            "x",
            "1 2",
            "4294967296",
        ] {
            assert!(Pages::parse(input, 3).is_none(), "{input}");
        }
    }
}
