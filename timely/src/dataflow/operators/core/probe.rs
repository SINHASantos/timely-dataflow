//! Monitor progress at a `Stream`.

use std::rc::Rc;
use std::cell::RefCell;

use crate::progress::Timestamp;
use crate::progress::frontier::{AntichainRef, MutableAntichain};
use crate::dataflow::channels::pushers::Counter as PushCounter;
use crate::dataflow::channels::pushers::buffer::Buffer as PushBuffer;
use crate::dataflow::channels::pact::Pipeline;
use crate::dataflow::channels::pullers::Counter as PullCounter;
use crate::dataflow::operators::generic::builder_raw::OperatorBuilder;


use crate::dataflow::{StreamCore, Scope};
use crate::{Container, Data};

/// Monitors progress at a `Stream`.
pub trait Probe<G: Scope, C: Container> {
    /// Constructs a progress probe which indicates which timestamps have elapsed at the operator.
    ///
    /// # Examples
    /// ```
    /// use timely::*;
    /// use timely::dataflow::Scope;
    /// use timely::dataflow::operators::{Input, Probe, Inspect};
    ///
    /// // construct and execute a timely dataflow
    /// timely::execute(Config::thread(), |worker| {
    ///
    ///     // add an input and base computation off of it
    ///     let (mut input, probe) = worker.dataflow(|scope| {
    ///         let (input, stream) = scope.new_input();
    ///         let probe = stream.inspect(|x| println!("hello {:?}", x))
    ///                           .probe();
    ///         (input, probe)
    ///     });
    ///
    ///     // introduce input, advance computation
    ///     for round in 0..10 {
    ///         input.send(round);
    ///         input.advance_to(round + 1);
    ///         worker.step_while(|| probe.less_than(input.time()));
    ///     }
    /// }).unwrap();
    /// ```
    fn probe(&self) -> Handle<G::Timestamp>;

    /// Inserts a progress probe in a stream.
    ///
    /// # Examples
    /// ```
    /// use timely::*;
    /// use timely::dataflow::Scope;
    /// use timely::dataflow::operators::{Input, Probe, Inspect};
    /// use timely::dataflow::operators::probe::Handle;
    ///
    /// // construct and execute a timely dataflow
    /// timely::execute(Config::thread(), |worker| {
    ///
    ///     // add an input and base computation off of it
    ///     let mut probe = Handle::new();
    ///     let mut input = worker.dataflow(|scope| {
    ///         let (input, stream) = scope.new_input();
    ///         stream.probe_with(&mut probe)
    ///               .inspect(|x| println!("hello {:?}", x));
    ///
    ///         input
    ///     });
    ///
    ///     // introduce input, advance computation
    ///     for round in 0..10 {
    ///         input.send(round);
    ///         input.advance_to(round + 1);
    ///         worker.step_while(|| probe.less_than(input.time()));
    ///     }
    /// }).unwrap();
    /// ```
    fn probe_with(&self, handle: &Handle<G::Timestamp>) -> StreamCore<G, C>;
}

impl<G: Scope, C: Container + Data> Probe<G, C> for StreamCore<G, C> {
    fn probe(&self) -> Handle<G::Timestamp> {

        // the frontier is shared state; scope updates, handle reads.
        let handle = Handle::<G::Timestamp>::new();
        self.probe_with(&handle);
        handle
    }
    fn probe_with(&self, handle: &Handle<G::Timestamp>) -> StreamCore<G, C> {

        let mut builder = OperatorBuilder::new("Probe".to_owned(), self.scope());
        let mut input = PullCounter::new(builder.new_input(self, Pipeline));
        let (tee, stream) = builder.new_output();
        let mut output = PushBuffer::new(PushCounter::new(tee));

        let shared_frontier = Rc::downgrade(&handle.frontier);
        let mut started = false;

        builder.build(
            move |progress| {

                // surface all frontier changes to the shared frontier.
                if let Some(shared_frontier) = shared_frontier.upgrade() {
                    let mut borrow = shared_frontier.borrow_mut();
                    borrow.update_iter(progress.frontiers[0].drain());
                }

                if !started {
                    // discard initial capability.
                    progress.internals[0].update(G::Timestamp::minimum(), -1);
                    started = true;
                }

                while let Some(message) = input.next() {
                    let time = &message.time;
                    let data = &mut message.data;
                    output.session(time).give_container(data);
                }
                output.cease();

                // extract what we know about progress from the input and output adapters.
                input.consumed().borrow_mut().drain_into(&mut progress.consumeds[0]);
                output.inner().produced().borrow_mut().drain_into(&mut progress.produceds[0]);

                false
            },
        );

        stream
    }
}

/// Reports information about progress at the probe.
#[derive(Debug)]
pub struct Handle<T:Timestamp> {
    frontier: Rc<RefCell<MutableAntichain<T>>>
}

impl<T: Timestamp> Handle<T> {
    /// Returns `true` iff the frontier is strictly less than `time`.
    #[inline] pub fn less_than(&self, time: &T) -> bool { self.frontier.borrow().less_than(time) }
    /// Returns `true` iff the frontier is less than or equal to `time`.
    #[inline] pub fn less_equal(&self, time: &T) -> bool { self.frontier.borrow().less_equal(time) }
    /// Returns `true` iff the frontier is empty.
    #[inline] pub fn done(&self) -> bool { self.frontier.borrow().is_empty() }
    /// Allocates a new handle.
    #[inline] pub fn new() -> Self { Handle { frontier: Rc::new(RefCell::new(MutableAntichain::new())) } }

    /// Invokes a method on the frontier, returning its result.
    ///
    /// This method allows inspection of the frontier, which cannot be returned by reference as
    /// it is on the other side of a `RefCell`.
    ///
    /// # Examples
    ///
    /// ```
    /// use timely::dataflow::operators::probe::Handle;
    ///
    /// let handle = Handle::<usize>::new();
    /// let frontier = handle.with_frontier(|frontier| frontier.to_vec());
    /// ```
    #[inline]
    pub fn with_frontier<R, F: FnMut(AntichainRef<T>)->R>(&self, mut function: F) -> R {
        function(self.frontier.borrow().frontier())
    }
}

impl<T: Timestamp> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Handle {
            frontier: Rc::clone(&self.frontier)
        }
    }
}

impl<T> Default for Handle<T>
where
    T: Timestamp,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {

    use crate::Config;
    use crate::dataflow::operators::{Input, Probe};

    #[test]
    fn probe() {

        // initializes and runs a timely dataflow computation
        crate::execute(Config::thread(), |worker| {

            // create a new input, and inspect its output
            let (mut input, probe) = worker.dataflow(move |scope| {
                let (input, stream) = scope.new_input::<String>();
                (input, stream.probe())
            });

            // introduce data and watch!
            for round in 0..10 {
                assert!(!probe.done());
                assert!(probe.less_equal(&round));
                assert!(probe.less_than(&(round + 1)));
                input.advance_to(round + 1);
                worker.step();
            }

            // seal the input
            input.close();

            // finish off any remaining work
            worker.step();
            worker.step();
            worker.step();
            worker.step();
            assert!(probe.done());
        }).unwrap();
    }

}
