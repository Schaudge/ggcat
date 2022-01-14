// use crate::colors::storage::roaring::ColorsStorage;
use crate::colors::storage::serializer::{ColorsFlushProcessing, ColorsSerializer};
use crate::colors::storage::ColorsSerializerImpl;
use crate::colors::ColorIndexType;
use crate::hashes::dummy_hasher::{DummyHasher, DummyHasherBuilder};
use crate::io::varint::{decode_varint, encode_varint};
use crate::utils::async_slice_queue::AsyncSliceQueue;
use crate::KEEP_FILES;
use byteorder::ReadBytesExt;
use dashmap::DashMap;
use parking_lot::Mutex;
use rand::{thread_rng, RngCore};
use siphasher::sip128::{Hash128, Hasher128, SipHasher13};
use std::cell::UnsafeCell;
use std::cmp::max;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Read, Write};
use std::mem::{swap, transmute};
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct ColorsMemMap<C: ColorsSerializerImpl> {
    colors: DashMap<u128, ColorIndexType, DummyHasherBuilder>,
    colors_storage: ColorsSerializer<C>,
    hash_keys: (u64, u64),
}

impl<C: ColorsSerializerImpl> ColorsMemMap<C> {
    pub fn new(file: impl AsRef<Path>, color_names: Vec<String>) -> Self {
        let mut rng = thread_rng();
        Self {
            colors: DashMap::with_hasher(DummyHasherBuilder),
            colors_storage: ColorsSerializer::new(file, color_names),
            hash_keys: (rng.next_u64(), rng.next_u64()),
        }
    }

    fn hash_colors(&self, colors: &[ColorIndexType]) -> u128 {
        let mut hasher = SipHasher13::new_with_keys(self.hash_keys.0, self.hash_keys.1);
        colors.hash(&mut hasher);
        hasher.finish128().as_u128()
    }

    pub fn get_id(&self, colors: &[ColorIndexType]) -> ColorIndexType {
        let hash = self.hash_colors(colors);

        match self.colors.get(&hash) {
            None => unsafe {
                let color = self.colors_storage.serialize_colors(colors);
                self.colors.insert(hash, color);
                color
            },
            Some(id) => *id,
        }
    }

    pub fn print_stats(&self) {
        self.colors_storage.print_stats();
    }
}