use std::hash::{Hash, Hasher};

use ahash::CallHasher;
use arrow::array::*;
use hashbrown::hash_map::RawEntryMut;
use polars_arrow::trusted_len::PushUnchecked;

use crate::frame::groupby::hashing::HASHMAP_INIT_SIZE;
use crate::prelude::*;
use crate::{datatypes::PlHashMap, use_string_cache, StrHashGlobal, StringCache, POOL};

pub enum RevMappingBuilder {
    /// Hashmap: maps the indexes from the global cache/categorical array to indexes in the local Utf8Array
    /// Utf8Array: caches the string values
    GlobalFinished(PlHashMap<u32, u32>, Utf8Array<i64>, u128),
    /// Utf8Array: caches the string values
    Local(MutableUtf8Array<i64>),
}

impl RevMappingBuilder {
    fn insert(&mut self, value: &str) {
        use RevMappingBuilder::*;
        match self {
            Local(builder) => builder.push(Some(value)),
            GlobalFinished(_, _, _) => {
                #[cfg(debug_assertions)]
                {
                    unreachable!()
                }
                #[cfg(not(debug_assertions))]
                {
                    use std::hint::unreachable_unchecked;
                    unsafe { unreachable_unchecked() }
                }
            }
        };
    }

    fn finish(self) -> RevMapping {
        use RevMappingBuilder::*;
        match self {
            Local(b) => RevMapping::Local(b.into()),
            GlobalFinished(map, b, uuid) => RevMapping::Global(map, b, uuid),
        }
    }
}

#[derive(Clone, Debug)]
pub enum RevMapping {
    /// Hashmap: maps the indexes from the global cache/categorical array to indexes in the local Utf8Array
    /// Utf8Array: caches the string values
    Global(PlHashMap<u32, u32>, Utf8Array<i64>, u128),
    /// Utf8Array: caches the string values
    Local(Utf8Array<i64>),
}

impl Default for RevMapping {
    fn default() -> Self {
        let slice: &[Option<&str>] = &[];
        RevMapping::Local(Utf8Array::<i64>::from(slice))
    }
}

#[allow(clippy::len_without_is_empty)]
impl RevMapping {
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global(_, _, _))
    }

    /// Get the length of the [`RevMapping`]
    pub fn len(&self) -> usize {
        match self {
            Self::Global(_, a, _) => a.len(),
            Self::Local(a) => a.len(),
        }
    }

    /// Categorical to str
    pub fn get(&self, idx: u32) -> &str {
        match self {
            Self::Global(map, a, _) => {
                let idx = *map.get(&idx).unwrap();
                a.value(idx as usize)
            }
            Self::Local(a) => a.value(idx as usize),
        }
    }

    /// Categorical to str
    ///
    /// # Safety
    /// This doesn't do any bound checking
    pub(crate) unsafe fn get_unchecked(&self, idx: u32) -> &str {
        match self {
            Self::Global(map, a, _) => {
                let idx = *map.get(&idx).unwrap();
                a.value_unchecked(idx as usize)
            }
            Self::Local(a) => a.value_unchecked(idx as usize),
        }
    }
    /// Check if the categoricals are created under the same global string cache.
    pub fn same_src(&self, other: &Self) -> bool {
        match (self, other) {
            (RevMapping::Global(_, _, l), RevMapping::Global(_, _, r)) => *l == *r,
            (RevMapping::Local(l), RevMapping::Local(r)) => {
                std::ptr::eq(l as *const Utf8Array<_>, r as *const Utf8Array<_>)
            }
            _ => false,
        }
    }

    /// str to Categorical
    pub fn find(&self, value: &str) -> Option<u32> {
        match self {
            Self::Global(map, a, _) => {
                map.iter()
                    // Safety:
                    // value is always within bounds
                    .find(|(_k, &v)| (unsafe { a.value_unchecked(v as usize) } == value))
                    .map(|(k, _v)| *k)
            }
            Self::Local(a) => {
                // Safety: within bounds
                unsafe { (0..a.len()).find(|idx| a.value_unchecked(*idx) == value) }
                    .map(|idx| idx as u32)
            }
        }
    }
}

#[derive(Eq, Copy, Clone)]
pub struct StrHashLocal<'a> {
    str: &'a str,
    hash: u64,
}

impl<'a> Hash for StrHashLocal<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash)
    }
}

impl<'a> StrHashLocal<'a> {
    pub(crate) fn new(s: &'a str, hash: u64) -> Self {
        Self { str: s, hash }
    }
}

impl<'a> PartialEq for StrHashLocal<'a> {
    fn eq(&self, other: &Self) -> bool {
        // can be collisions in the hashtable even though the hashes are equal
        // e.g. hashtable hash = hash % n_slots
        (self.hash == other.hash) && (self.str == other.str)
    }
}

pub struct CategoricalChunkedBuilder {
    cat_builder: UInt32Vec,
    name: String,
    reverse_mapping: RevMappingBuilder,
}

impl CategoricalChunkedBuilder {
    pub fn new(name: &str, capacity: usize) -> Self {
        let builder = MutableUtf8Array::<i64>::with_capacity(capacity / 10);
        let reverse_mapping = RevMappingBuilder::Local(builder);

        Self {
            cat_builder: UInt32Vec::with_capacity(capacity),
            name: name.to_string(),
            reverse_mapping,
        }
    }
}
impl CategoricalChunkedBuilder {
    ///
    /// `store_hashes` is not needed by the local builder, only for the global builder under contention
    /// The hashes have the same order as the `Utf8Array` values.
    fn build_local_map<'a, I>(&mut self, i: I, store_hashes: bool) -> Vec<u64>
    where
        I: IntoIterator<Item = Option<&'a str>>,
    {
        let mut iter = i.into_iter();
        let mut hashes = if store_hashes {
            Vec::with_capacity(iter.size_hint().0 / 10)
        } else {
            vec![]
        };
        // It is important that we use the same hash builder as the global `StringCache` does.
        let mut mapping =
            PlHashMap::with_capacity_and_hasher(HASHMAP_INIT_SIZE, StringCache::get_hash_builder());
        let hb = mapping.hasher().clone();
        for opt_s in &mut iter {
            match opt_s {
                Some(s) => {
                    let h = str::get_hash(s, &hb);
                    let key = StrHashLocal::new(s, h);
                    let mut idx = mapping.len() as u32;

                    let entry = mapping.raw_entry_mut().from_key_hashed_nocheck(h, &key);

                    match entry {
                        RawEntryMut::Occupied(entry) => idx = *entry.get(),
                        RawEntryMut::Vacant(entry) => {
                            if store_hashes {
                                hashes.push(h)
                            }
                            entry.insert_with_hasher(h, key, idx, |s| s.hash);
                            self.reverse_mapping.insert(s);
                        }
                    };
                    self.cat_builder.push(Some(idx));
                }
                None => {
                    self.cat_builder.push(None);
                }
            }
        }

        if mapping.len() > u32::MAX as usize {
            panic!("not more than {} categories supported", u32::MAX)
        };
        hashes
    }

    /// Build a global string cached `CategoricalChunked` from a local `Dictionary`.
    pub(super) fn global_map_from_local(&mut self, keys: &UInt32Array, values: Utf8Array<i64>) {
        // locally we don't need a hashmap because we all categories are 1 integer apart
        // so the index is local, and the values is global
        let mut local_to_global: Vec<u32> = Vec::with_capacity(values.len());
        let id;

        // now we have to lock the global string cache.
        // we will create a mapping from our local categoricals to global categoricals
        // and a mapping from global categoricals to our local categoricals

        // in a separate scope so that we drop the global cache as soon as we are finished
        {
            let cache = &mut crate::STRING_CACHE.lock_map();
            id = cache.uuid;
            let global_mapping = &mut cache.map;
            let hb = global_mapping.hasher().clone();

            for s in values.values_iter() {
                let h = str::get_hash(s, &hb);
                let mut global_idx = global_mapping.len() as u32;
                // Note that we don't create the StrHashGlobal to search the key in the hashmap
                // as StrHashGlobal may allocate a string
                let entry = global_mapping
                    .raw_entry_mut()
                    .from_hash(h, |val| (val.hash == h) && val.str == s);

                match entry {
                    RawEntryMut::Occupied(entry) => global_idx = *entry.get(),
                    RawEntryMut::Vacant(entry) => {
                        // only just now we allocate the string
                        let key = StrHashGlobal::new(s.into(), h);
                        entry.insert_with_hasher(h, key, global_idx, |s| s.hash);
                    }
                }
                // safety:
                // we allocated enough
                unsafe { local_to_global.push_unchecked(global_idx) }
            }
            if global_mapping.len() > u32::MAX as usize {
                panic!("not more than {} categories supported", u32::MAX)
            };
        }
        // we now know the exact size
        // no reallocs
        let mut global_to_local = PlHashMap::with_capacity(local_to_global.len());

        let compute_cats = || {
            if local_to_global.is_empty() {
                Default::default()
            } else {
                keys.into_iter()
                    .map(|opt_k| {
                        opt_k.map(|cat| {
                            debug_assert!((*cat as usize) < local_to_global.len());
                            *unsafe { local_to_global.get_unchecked(*cat as usize) }
                        })
                    })
                    .collect::<UInt32Vec>()
            }
        };

        let (_, cats) = POOL.join(
            || fill_global_to_local(&local_to_global, &mut global_to_local),
            compute_cats,
        );
        self.cat_builder = cats;

        self.reverse_mapping = RevMappingBuilder::GlobalFinished(global_to_local, values, id)
    }

    fn build_global_map_contention<'a, I>(&mut self, i: I)
    where
        I: IntoIterator<Item = Option<&'a str>>,
    {
        // first build the values: `Utf8Array`
        // we can use a local hashmap for that
        // `hashes.len()` is equal to to the number of unique values.
        let hashes = self.build_local_map(i, true);

        // locally we don't need a hashmap because we all categories are 1 integer apart
        // so the index is local, and the values is global
        let mut local_to_global: Vec<u32>;
        let id;

        // now we have to lock the global string cache.
        // we will create a mapping from our local categoricals to global categoricals
        // and a mapping from global categoricals to our local categoricals
        let values: Utf8Array<_> =
            if let RevMappingBuilder::Local(values) = &mut self.reverse_mapping {
                debug_assert_eq!(hashes.len(), values.len());
                // resize local now that we know the size of the mapping.
                local_to_global = Vec::with_capacity(values.len());
                std::mem::take(values).into()
            } else {
                unreachable!()
            };

        // in a separate scope so that we drop the global cache as soon as we are finished
        {
            let cache = &mut crate::STRING_CACHE.lock_map();
            id = cache.uuid;
            let global_mapping = &mut cache.map;

            for (s, h) in values.values_iter().zip(hashes.into_iter()) {
                let mut global_idx = global_mapping.len() as u32;
                // Note that we don't create the StrHashGlobal to search the key in the hashmap
                // as StrHashGlobal may allocate a string
                let entry = global_mapping
                    .raw_entry_mut()
                    .from_hash(h, |val| (val.hash == h) && val.str == s);

                match entry {
                    RawEntryMut::Occupied(entry) => global_idx = *entry.get(),
                    RawEntryMut::Vacant(entry) => {
                        // only just now we allocate the string
                        let key = StrHashGlobal::new(s.into(), h);
                        entry.insert_with_hasher(h, key, global_idx, |s| s.hash);
                    }
                }
                // safety:
                // we allocated enough
                unsafe { local_to_global.push_unchecked(global_idx) }
            }
            if global_mapping.len() > u32::MAX as usize {
                panic!("not more than {} categories supported", u32::MAX)
            };
        }
        // we now know the exact size
        // no reallocs
        let mut global_to_local = PlHashMap::with_capacity(local_to_global.len());

        let update_cats = || {
            if !local_to_global.is_empty() {
                // when all categorical are null, `local_to_global` is empty and all cats physical values are 0.
                self.cat_builder.apply_values(|cats| {
                    for cat in cats {
                        debug_assert!((*cat as usize) < local_to_global.len());
                        *cat = *unsafe { local_to_global.get_unchecked(*cat as usize) };
                    }
                })
            };
        };

        POOL.join(
            || fill_global_to_local(&local_to_global, &mut global_to_local),
            update_cats,
        );

        self.reverse_mapping = RevMappingBuilder::GlobalFinished(global_to_local, values, id)
    }

    /// Appends all the values in a single lock of the global string cache.
    pub fn drain_iter<'a, I>(&mut self, i: I)
    where
        I: IntoIterator<Item = Option<&'a str>>,
    {
        if use_string_cache() {
            self.build_global_map_contention(i)
        } else {
            let _ = self.build_local_map(i, false);
        }
    }

    pub fn finish(mut self) -> CategoricalChunked {
        CategoricalChunked::from_chunks_original(
            &self.name,
            vec![self.cat_builder.as_box()],
            self.reverse_mapping.finish(),
        )
    }
}

fn fill_global_to_local(local_to_global: &[u32], global_to_local: &mut PlHashMap<u32, u32>) {
    let mut local_idx = 0;
    #[allow(clippy::explicit_counter_loop)]
    for global_idx in local_to_global {
        // we know the keys are unique so this is much faster
        global_to_local.insert_unique_unchecked(*global_idx, local_idx);
        local_idx += 1;
    }
}

#[cfg(test)]
mod test {
    use crate::chunked_array::categorical::CategoricalChunkedBuilder;
    use crate::prelude::*;
    use crate::{reset_string_cache, toggle_string_cache, SINGLE_LOCK};

    #[test]
    fn test_categorical_rev() -> Result<()> {
        let _lock = SINGLE_LOCK.lock();
        reset_string_cache();
        let slice = &[
            Some("foo"),
            None,
            Some("bar"),
            Some("foo"),
            Some("foo"),
            Some("bar"),
        ];
        let ca = Utf8Chunked::new("a", slice);
        let out = ca.cast(&DataType::Categorical(None))?;
        let out = out.categorical().unwrap().clone();
        assert_eq!(out.get_rev_map().len(), 2);

        // test the global branch
        toggle_string_cache(true);
        // empty global cache
        let out = ca.cast(&DataType::Categorical(None))?;
        let out = out.categorical().unwrap().clone();
        assert_eq!(out.get_rev_map().len(), 2);
        // full global cache
        let out = ca.cast(&DataType::Categorical(None))?;
        let out = out.categorical().unwrap().clone();
        assert_eq!(out.get_rev_map().len(), 2);

        // Check that we don't panic if we append two categorical arrays
        // build under the same string cache
        // https://github.com/pola-rs/polars/issues/1115
        let ca1 = Utf8Chunked::new("a", slice).cast(&DataType::Categorical(None))?;
        let mut ca1 = ca1.categorical().unwrap().clone();
        let ca2 = Utf8Chunked::new("a", slice).cast(&DataType::Categorical(None))?;
        let ca2 = ca2.categorical().unwrap();
        ca1.append(ca2).unwrap();

        Ok(())
    }

    #[test]
    fn test_categorical_builder() {
        use crate::{reset_string_cache, toggle_string_cache};
        let _lock = crate::SINGLE_LOCK.lock();
        for b in &[false, true] {
            reset_string_cache();
            toggle_string_cache(*b);

            // Use 2 builders to check if the global string cache
            // does not interfere with the index mapping
            let mut builder1 = CategoricalChunkedBuilder::new("foo", 10);
            let mut builder2 = CategoricalChunkedBuilder::new("foo", 10);
            builder1.drain_iter(vec![None, Some("hello"), Some("vietnam")]);
            builder2.drain_iter(vec![Some("hello"), None, Some("world")].into_iter());

            let s = builder1.finish().into_series();
            assert_eq!(s.str_value(0), "null");
            assert_eq!(s.str_value(1), "hello");
            assert_eq!(s.str_value(2), "vietnam");

            let s = builder2.finish().into_series();
            assert_eq!(s.str_value(0), "hello");
            assert_eq!(s.str_value(1), "null");
            assert_eq!(s.str_value(2), "world");
        }
    }
}
