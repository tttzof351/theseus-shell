//! Retained heap budget, independent of CPU speed and the system allocator's RSS arenas.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use theseus::terminal_renderer::{
    IndexedPhysicalLayout, RenderLine, TerminalSize, VirtualCursor, VirtualScreen,
};

struct MeteredAllocator;
static LIVE_BYTES: AtomicIsize = AtomicIsize::new(0);
unsafe impl GlobalAlloc for MeteredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, old, size) };
        if !next.is_null() {
            LIVE_BYTES.fetch_add(size as isize - old.size() as isize, Ordering::Relaxed);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: MeteredAllocator = MeteredAllocator;

// This binary deliberately has one test: no concurrent test allocations contaminate the delta.
#[test]
fn two_mb_of_retained_output_fits_in_32_mib_of_renderer_heap() {
    let before = LIVE_BYTES.load(Ordering::Relaxed);
    let lines = (0..10)
        .map(|_| {
            let text = (0..200_000)
                .map(|n| char::from(b'!' + (n % 90) as u8))
                .collect::<String>();
            RenderLine::plain(text)
        })
        .chain(std::iter::once(RenderLine::plain("editor")))
        .collect::<Vec<_>>();
    let mut screen = VirtualScreen::from_render_lines(
        lines,
        VirtualCursor {
            line: 10,
            char_offset: 6,
        },
        true,
    );
    let mut layout = IndexedPhysicalLayout::default();
    let terminal = layout.layout(&screen, TerminalSize::new(120, 40));
    assert!(
        layout.heights.total() > 16_000,
        "all output must remain available"
    );
    assert_eq!(terminal.rows.len(), 40);
    screen.dirty_from = 10;
    let _ = layout.layout(&screen, TerminalSize::new(120, 40));
    assert_eq!(layout.reflowed_last_frame, 0);
    let bytes = LIVE_BYTES.load(Ordering::Relaxed) - before;
    eprintln!("retained renderer heap for 2 MB: {bytes} bytes");
    assert!(
        bytes < 32 * 1024 * 1024,
        "2 MB output retained {bytes} bytes (budget: 32 MiB)"
    );
}
