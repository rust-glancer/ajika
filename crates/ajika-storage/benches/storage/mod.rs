//! Storage benchmarks measure the CPU work around values and checkpoints. Filesystem waiting
//! is excluded by CodSpeed simulation, so these are not disk latency or throughput measurements.

mod checkpoints;
mod maintenance;
mod payloads;
mod support;

fn main() {
    divan::main();
}
