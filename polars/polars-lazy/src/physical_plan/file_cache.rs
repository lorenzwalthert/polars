use parking_lot::Mutex;
use polars_core::prelude::*;

use crate::prelude::file_caching::FileFingerPrint;
use crate::prelude::*;

#[derive(Clone)]
pub(crate) struct FileCache {
    // (path, predicate) -> (read_count, df)
    inner: Arc<PlHashMap<FileFingerPrint, Mutex<(FileCount, DataFrame)>>>,
}

impl FileCache {
    pub(super) fn new(finger_prints: Option<Vec<FileFingerPrint>>) -> Self {
        let inner = match finger_prints {
            None => Arc::new(Default::default()),
            Some(fps) => {
                let mut mapping = PlHashMap::with_capacity(fps.len());
                for fp in fps {
                    mapping.insert(fp, Mutex::new((0, Default::default())));
                }
                Arc::new(mapping)
            }
        };

        Self { inner }
    }
    pub(crate) fn read<F>(
        &self,
        finger_print: FileFingerPrint,
        total_read_count: FileCount,
        reader: &mut F,
    ) -> Result<DataFrame>
    where
        F: FnMut() -> Result<DataFrame>,
    {
        if total_read_count == 1 {
            if total_read_count == 0 {
                eprintln!("we have hit an unexpected branch, please open an issue")
            }
            reader()
        } else {
            // should exist
            let guard = self.inner.get(&finger_print).unwrap();
            let mut state = guard.lock();

            // initialize df
            if state.0 == 0 {
                state.1 = reader()?;
            }
            state.0 += 1;

            // remove dataframe from memory
            if state.0 == total_read_count {
                Ok(std::mem::take(&mut state.1))
            } else {
                Ok(state.1.clone())
            }
        }
    }
}
