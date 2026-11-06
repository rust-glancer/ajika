//! Each module describes one way an application uses the same query graph.
//! Fixture construction and scenario checks are outside the measured operation.

mod graph;
mod inputs;
mod outputs;
mod persistence;
mod sessions;
mod support;

fn main() {
    divan::main();
}
