use std::sync::{
    Arc, Once, OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[cfg(unix)]
use signal_hook::{consts::signal::SIGINT, flag as signal_flag};

static SIGINT_REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static INSTALL_SIGINT_HANDLER: Once = Once::new();

#[derive(Debug, Clone, Default)]
pub(crate) struct CancellationEvent {
    cancelled: Arc<AtomicBool>,
}

impl CancellationEvent {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel_if_interrupted(&self) -> bool {
        if take_sigint_request() {
            self.cancel();
        }

        self.is_cancelled()
    }
}

pub(crate) fn install_sigint_handler() {
    INSTALL_SIGINT_HANDLER.call_once(|| {
        #[cfg(unix)]
        let _ = signal_flag::register(SIGINT, Arc::clone(sigint_requested()));
    });
}

pub(crate) fn clear_sigint_request() {
    sigint_requested().store(false, Ordering::SeqCst);
}

pub(crate) fn take_sigint_request() -> bool {
    sigint_requested().swap(false, Ordering::SeqCst)
}

fn sigint_requested() -> &'static Arc<AtomicBool> {
    SIGINT_REQUESTED.get_or_init(|| Arc::new(AtomicBool::new(false)))
}
