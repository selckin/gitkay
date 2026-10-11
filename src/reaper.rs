//! The `gitkay-drop` thread: where the UI thread sends what it lets go of, so the
//! freeing happens off the frame loop. Freeing a diff is a pass over every row's
//! allocation — 3.24s measured on a 76.5M-line one — and even a few hundred thousand
//! lines is a dropped frame. Knows nothing of diffs: anything `Send` can be disposed.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};

/// A value on its way to be dropped; boxed so one channel carries every type.
type Garbage = Box<dyn Send>;

/// The drop thread's end of the channel, made on first use. `None` when the thread
/// could not be spawned, which every later call then reads as "drop inline".
static DROP_TX: OnceLock<Option<Sender<Garbage>>> = OnceLock::new();

/// Hand `value` to the drop thread, which frees it. Spawned on first use; when the
/// spawn failed or the thread is gone, the value is dropped here instead — a slower
/// frame, never a panic. Unconditional on size: a send is ~100ns, and a threshold
/// below which callers drop inline would be one more rule to keep right.
pub fn dispose<T: Send + 'static>(value: T) {
    send_or_drop(DROP_TX.get_or_init(spawn).as_ref(), value);
}

/// `dispose` for what a cache let go of: nothing to send when it let go of nothing,
/// which is most inserts, so the common case costs no box, no send and no thread.
pub fn dispose_all<T: Send + 'static>(values: Vec<T>) {
    if !values.is_empty() {
        dispose(values);
    }
}

/// Send `value` to the drop thread behind `tx`, or drop it here when there is none or
/// it has exited (a send fails only then, handing the value back).
fn send_or_drop<T: Send + 'static>(tx: Option<&Sender<Garbage>>, value: T) {
    let garbage: Garbage = Box::new(value);
    match tx {
        Some(tx) => {
            if let Err(mpsc::SendError(garbage)) = tx.send(garbage) {
                drop(garbage);
            }
        }
        None => drop(garbage),
    }
}

/// Start `gitkay-drop`. Never joined: process exit frees everything, and waiting for
/// it would slow shutdown.
fn spawn() -> Option<Sender<Garbage>> {
    let (tx, rx) = mpsc::channel::<Garbage>();
    match std::thread::Builder::new()
        .name("gitkay-drop".into())
        .spawn(move || rx.into_iter().for_each(drop))
    {
        Ok(_) => Some(tx),
        Err(e) => {
            log::warn!("could not start the drop thread ({e}); freeing diffs inline");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::{self, ThreadId};
    use std::time::Duration;

    /// Reports, from its `Drop`, the thread it was dropped on and that thread's name.
    struct Signal(mpsc::Sender<(ThreadId, Option<String>)>);

    impl Drop for Signal {
        fn drop(&mut self) {
            let me = thread::current();
            // The receiver outlives every test's wait; a failed send only means the
            // test already gave up, which its own assertion reports.
            self.0
                .send((me.id(), me.name().map(String::from)))
                .unwrap_or_default();
        }
    }

    fn signal() -> (Signal, mpsc::Receiver<(ThreadId, Option<String>)>) {
        let (tx, rx) = mpsc::channel();
        (Signal(tx), rx)
    }

    #[test]
    fn a_disposed_value_is_dropped_on_the_drop_thread() {
        let (value, dropped) = signal();
        dispose(value);
        let (id, name) = dropped
            .recv_timeout(Duration::from_secs(10))
            .expect("the disposed value was dropped");
        assert_ne!(
            id,
            thread::current().id(),
            "dropped off the caller's thread"
        );
        assert_eq!(name.as_deref(), Some("gitkay-drop"));
    }

    #[test]
    fn dispose_all_sends_what_a_cache_let_go_of() {
        let (value, dropped) = signal();
        dispose_all(vec![value]);
        let (_, name) = dropped
            .recv_timeout(Duration::from_secs(10))
            .expect("the disposed value was dropped");
        assert_eq!(name.as_deref(), Some("gitkay-drop"));
    }

    #[test]
    fn with_no_drop_thread_the_value_is_dropped_inline() {
        let (value, dropped) = signal();
        send_or_drop(None, value);
        let (id, _) = dropped
            .try_recv()
            .expect("dropped before the call returned");
        assert_eq!(id, thread::current().id());
    }

    #[test]
    fn with_the_drop_thread_gone_the_value_is_dropped_inline() {
        let (tx, rx) = mpsc::channel::<Garbage>();
        drop(rx); // what a drop thread that has exited leaves behind
        let (value, dropped) = signal();
        send_or_drop(Some(&tx), value);
        let (id, _) = dropped
            .try_recv()
            .expect("dropped before the call returned");
        assert_eq!(id, thread::current().id());
    }
}
