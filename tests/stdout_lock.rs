use std::{
    io::{self, Write},
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use theseus::common::terminal_output::with_locked_writer;

#[test]
fn spinner_frames_do_not_interleave_inside_streaming_transaction() -> io::Result<()> {
    let lock = Arc::new(Mutex::new(()));
    let output = Arc::new(Mutex::new(Vec::new()));
    let streaming_started = Arc::new(Barrier::new(2));
    let stop_spinner = Arc::new(AtomicBool::new(false));

    let spinner_thread = {
        let lock = Arc::clone(&lock);
        let output = Arc::clone(&output);
        let streaming_started = Arc::clone(&streaming_started);
        let stop_spinner = Arc::clone(&stop_spinner);

        thread::spawn(move || {
            streaming_started.wait();
            while !stop_spinner.load(Ordering::Relaxed) {
                with_locked_writer(&lock, &output, |output| {
                    output.write_all(b"\rS")?;
                    output.flush()
                })
                .unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        })
    };

    with_locked_writer(&lock, &output, |output| {
        output.write_all(b"BEGIN")?;
        streaming_started.wait();
        thread::sleep(Duration::from_millis(25));
        output.write_all(b"END")?;
        output.flush()
    })?;

    stop_spinner.store(true, Ordering::Relaxed);
    spinner_thread.join().unwrap();

    let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    let begin = output.find("BEGIN").unwrap();
    let end = output[begin..]
        .find("END")
        .map(|offset| begin + offset)
        .unwrap();
    let transaction = &output[begin..end + "END".len()];

    assert_eq!(
        transaction, "BEGINEND",
        "spinner frame interleaved inside streaming transaction: {output:?}"
    );
    assert!(
        output[end + "END".len()..].contains("\rS"),
        "test did not exercise a spinner frame after the streaming lock was released: {output:?}"
    );

    Ok(())
}
