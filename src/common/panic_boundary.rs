//! A caught backend panic is an operation error, not an independent stderr writer.

use std::{
    cell::Cell,
    panic::{self, AssertUnwindSafe},
    sync::Once,
};

thread_local! { static CAUGHT_BACKEND: Cell<bool> = const { Cell::new(false) }; }

pub(crate) fn catch<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !CAUGHT_BACKEND.with(Cell::get) {
                previous(info);
            }
        }));
    });
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAUGHT_BACKEND.with(|flag| flag.set(self.0));
        }
    }
    let _restore = Restore(CAUGHT_BACKEND.with(|flag| flag.replace(true)));
    panic::catch_unwind(AssertUnwindSafe(work)).map_err(|payload| {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
            })
            .unwrap_or_else(|| "unknown panic payload".into())
    })
}
