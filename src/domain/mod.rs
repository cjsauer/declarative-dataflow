//! Logic for working with attributes under a shared timestamp
//! semantics.

use std::collections::HashMap;

use timely::dataflow::operators::{Filter, Map};
use timely::dataflow::{ProbeHandle, Scope, Stream};
use timely::order::TotalOrder;
use timely::progress::Timestamp;

use differential_dataflow::input::{Input, InputSession};
use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::Threshold;
use differential_dataflow::AsCollection;

use crate::CollectionIndex;
use crate::{Aid, Error, TxData, Value};

/// A domain manages attributes (and their inputs) hat share a
/// timestamp semantics (e.g. come from the same logical source).
pub struct Domain<T: Timestamp + Lattice + TotalOrder> {
    /// The current timestamp.
    now_at: T,
    /// Input handles to attributes in this domain.
    input_sessions: HashMap<String, InputSession<T, (Value, Value), isize>>,
    /// The probe keeping track of progress in this domain.
    probe: ProbeHandle<T>,
    /// Forward attribute indices eid -> v.
    pub forward: HashMap<Aid, CollectionIndex<Value, Value, T>>,
    /// Reverse attribute indices v -> eid.
    pub reverse: HashMap<Aid, CollectionIndex<Value, Value, T>>,
}

impl<T> Domain<T>
where
    T: Timestamp + Lattice + TotalOrder,
{
    /// Creates a new domain.
    pub fn new(start_at: T) -> Self {
        Domain {
            now_at: start_at,
            input_sessions: HashMap::new(),
            probe: ProbeHandle::new(),
            forward: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    /// Creates a new collection of (e,v) tuples and indexes it in
    /// various ways. Stores forward, and reverse indices, as well as
    /// the input handle in the server state.
    pub fn create_attribute<S: Scope<Timestamp = T>>(
        &mut self,
        name: &str,
        scope: &mut S,
    ) -> Result<(), Error> {
        if self.forward.contains_key(name) {
            Err(Error {
                category: "df.error.category/conflict",
                message: format!("An attribute of name {} already exists.", name),
            })
        } else {
            let (handle, mut tuples) = scope.new_collection::<(Value, Value), isize>();

            // Ensure that redundant (e,v) pairs don't cause
            // misleading proposals during joining.
            tuples = tuples.distinct();

            let forward = CollectionIndex::index(name, &tuples);
            let reverse = CollectionIndex::index(name, &tuples.map(|(e, v)| (v, e)));

            self.forward.insert(name.to_string(), forward);
            self.reverse.insert(name.to_string(), reverse);

            self.input_sessions.insert(name.to_string(), handle);

            Ok(())
        }
    }

    /// Creates attributes from an external datoms source.
    pub fn create_source<S: Scope<Timestamp = T>>(
        &mut self,
        name: &str,
        name_idx: Option<usize>,
        datoms: &Stream<S, (usize, ((Value, Value), T, isize))>,
    ) -> Result<(), Error> {
        if self.forward.contains_key(name) {
            Err(Error {
                category: "df.error.category/conflict",
                message: format!("An attribute of name {} already exists.", name),
            })
        } else {
            let datoms = match name_idx {
                None => datoms.map(|(_idx, tuple)| tuple),
                Some(name_idx) => datoms
                    .filter(move |(idx, _tuple)| *idx == name_idx)
                    .map(|(_idx, tuple)| tuple),
            };

            let tuples = datoms
                .as_collection()
                // Ensure that redundant (e,v) pairs don't cause
                // misleading proposals during joining.
                .distinct();

            let forward = CollectionIndex::index(&name, &tuples);
            let reverse = CollectionIndex::index(&name, &tuples.map(|(e, v)| (v, e)));

            self.forward.insert(name.to_string(), forward);
            self.reverse.insert(name.to_string(), reverse);

            Ok(())
        }
    }

    /// Transact data into one or more inputs.
    pub fn transact(&mut self, tx_data: Vec<TxData>) -> Result<(), Error> {
        // @TODO do this smarter, e.g. grouped by handle
        for TxData(op, e, a, v) in tx_data {
            match self.input_sessions.get_mut(&a) {
                None => {
                    return Err(Error {
                        category: "df.error.category/not-found",
                        message: format!("Attribute {} does not exist.", a),
                    });
                }
                Some(handle) => {
                    handle.update((Value::Eid(e), v), op);
                }
            }
        }

        Ok(())
    }

    /// Closes and drops an existing input.
    pub fn close_input(&mut self, name: String) -> Result<(), Error> {
        match self.input_sessions.remove(&name) {
            None => Err(Error {
                category: "df.error.category/not-found",
                message: format!("Input {} does not exist.", name),
            }),
            Some(handle) => {
                handle.close();
                Ok(())
            }
        }
    }

    /// Advances the domain to `next`. The `trace_next` parameter can
    /// be used to indicate whether (and if so how closely) traces
    /// should follow the input frontier. Setting this to None
    /// maintains full trace histories.
    pub fn advance_to(&mut self, next: T, trace_next: Option<T>) {
        // Assert that we do not rewind time.
        assert!(self.now_at.less_equal(&next));

        if !self.now_at.eq(&next) {
            self.now_at = next.clone();

            for handle in self.input_sessions.values_mut() {
                handle.advance_to(next.clone());
                handle.flush();
            }

            if let Some(trace_next) = trace_next {
                // if historical queries don't matter, we should advance
                // the index traces to allow them to compact

                let frontier = &[trace_next];

                for index in self.forward.values_mut() {
                    index.advance_by(frontier);
                }

                for index in self.reverse.values_mut() {
                    index.advance_by(frontier);
                }
            }
        }
    }

    /// Reports the current timestamp.
    pub fn time(&self) -> &T {
        &self.now_at
    }
}