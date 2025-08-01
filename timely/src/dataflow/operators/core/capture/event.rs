//! Traits and types describing timely dataflow events.
//!
//! The `Event` type describes the information an operator can observe about a timely dataflow
//! stream. There are two types of events, (i) the receipt of data and (ii) reports of progress
//! of timestamps.

use columnar::Columnar;
use serde::{Deserialize, Serialize};

/// Data and progress events of the captured stream.
#[derive(Debug, Clone, Hash, Ord, PartialOrd, Eq, PartialEq, Deserialize, Serialize, Columnar)]
pub enum Event<T, C> {
    /// Progress received via `push_external_progress`.
    Progress(Vec<(T, i64)>),
    /// Messages received via the data stream.
    Messages(T, C),
}

/// Iterates over contained `Event<T, C>`.
///
/// The `EventIterator` trait describes types that can iterate over `Cow`s of events,
/// and which can be used to replay a stream into a new timely dataflow computation.
///
/// This method is not simply an iterator because of the lifetime in the result.
pub trait EventIterator<T: Clone, C: Clone> {
    /// Iterates over `Cow<Event<T, C>>` elements.
    fn next(&mut self) -> Option<std::borrow::Cow<'_, Event<T, C>>>;
}

/// Receives `Event<T, C>` events.
pub trait EventPusher<T, C> {
    /// Provides a new `Event<T, D>` to the pusher.
    fn push(&mut self, event: Event<T, C>);
}

// implementation for the linked list behind a `Handle`.
impl<T, C> EventPusher<T, C> for ::std::sync::mpsc::Sender<Event<T, C>> {
    fn push(&mut self, event: Event<T, C>) {
        // NOTE: An Err(x) result just means "data not accepted" most likely
        //       because the receiver is gone. No need to panic.
        let _ = self.send(event);
    }
}

/// A linked-list event pusher and iterator.
pub mod link {

    use std::borrow::Cow;
    use std::rc::Rc;
    use std::cell::RefCell;

    use super::{Event, EventPusher, EventIterator};

    /// A linked list of Event<T, C>.
    pub struct EventLink<T, C> {
        /// An event, if one exists.
        ///
        /// An event might not exist, if either we want to insert a `None` and have the output iterator pause,
        /// or in the case of the very first linked list element, which has no event when constructed.
        pub event: Option<Event<T, C>>,
        /// The next event, if it exists.
        pub next: RefCell<Option<Rc<EventLink<T, C>>>>,
    }

    impl<T, C> EventLink<T, C> {
        /// Allocates a new `EventLink`.
        pub fn new() -> EventLink<T, C> {
            EventLink { event: None, next: RefCell::new(None) }
        }
    }

    // implementation for the linked list behind a `Handle`.
    impl<T, C> EventPusher<T, C> for Rc<EventLink<T, C>> {
        fn push(&mut self, event: Event<T, C>) {
            *self.next.borrow_mut() = Some(Rc::new(EventLink { event: Some(event), next: RefCell::new(None) }));
            let next = Rc::clone(self.next.borrow().as_ref().unwrap());
            *self = next;
        }
    }

    impl<T: Clone, C: Clone> EventIterator<T, C> for Rc<EventLink<T, C>> {
        fn next(&mut self) -> Option<Cow<'_, Event<T, C>>> {
            let is_some = self.next.borrow().is_some();
            if is_some {
                let next = Rc::clone(self.next.borrow().as_ref().unwrap());
                *self = next;
                if let Some(this) = Rc::get_mut(self) {
                    this.event.take().map(Cow::Owned)
                }
                else {
                    self.event.as_ref().map(Cow::Borrowed)
                }
            }
            else {
                None
            }
        }
    }

    // Drop implementation to prevent stack overflow through naive drop impl.
    impl<T, C> Drop for EventLink<T, C> {
        fn drop(&mut self) {
            while let Some(link) = self.next.replace(None) {
                if let Ok(head) = Rc::try_unwrap(link) {
                    *self = head;
                }
            }
        }
    }

    impl<T, C> Default for EventLink<T, C> {
        fn default() -> Self {
            Self::new()
        }
    }

    #[test]
    fn avoid_stack_overflow_in_drop() {
        #[cfg(miri)]
        let limit = 1_000;
        #[cfg(not(miri))]
        let limit = 1_000_000;
        let mut event1 = Rc::new(EventLink::<(),()>::new());
        let _event2 = Rc::clone(&event1);
        for _ in 0 .. limit {
            event1.push(Event::Progress(vec![]));
        }
    }
}

/// A binary event pusher and iterator.
pub mod binary {

    use std::borrow::Cow;

    use serde::{de::DeserializeOwned, Serialize};

    use super::{Event, EventPusher, EventIterator};

    /// A wrapper for `W: Write` implementing `EventPusher<T, C>`.
    pub struct EventWriter<T, C, W: ::std::io::Write> {
        stream: W,
        phant: ::std::marker::PhantomData<(T, C)>,
    }

    impl<T, C, W: ::std::io::Write> EventWriter<T, C, W> {
        /// Allocates a new `EventWriter` wrapping a supplied writer.
        pub fn new(w: W) -> Self {
            Self {
                stream: w,
                phant: ::std::marker::PhantomData,
            }
        }
    }

    impl<T: Serialize, C: Serialize, W: ::std::io::Write> EventPusher<T, C> for EventWriter<T, C, W> {
        fn push(&mut self, event: Event<T, C>) {
            // TODO: `push` has no mechanism to report errors, so we `unwrap`.
            ::bincode::serialize_into(&mut self.stream, &event).expect("Event bincode/write failed");
        }
    }

    /// A Wrapper for `R: Read` implementing `EventIterator<T, D>`.
    pub struct EventReader<T, C, R: ::std::io::Read> {
        reader: R,
        decoded: Option<Event<T, C>>,
    }

    impl<T, C, R: ::std::io::Read> EventReader<T, C, R> {
        /// Allocates a new `EventReader` wrapping a supplied reader.
        pub fn new(r: R) -> Self {
            Self {
                reader: r,
                decoded: None,
            }
        }
    }

    impl<T: DeserializeOwned + Clone, C: DeserializeOwned + Clone, R: ::std::io::Read> EventIterator<T, C> for EventReader<T, C, R> {
        fn next(&mut self) -> Option<Cow<'_, Event<T, C>>> {
            self.decoded = ::bincode::deserialize_from(&mut self.reader).ok();
            self.decoded.take().map(Cow::Owned)
        }
    }
}
