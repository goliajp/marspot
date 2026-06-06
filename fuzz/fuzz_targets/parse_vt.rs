#![no_main]
//! Fuzz the VT/xterm escape parser with arbitrary bytes.
//!
//! Contract under test: `Parser::advance` must never panic on any
//! byte sequence. A no-op callback sink isolates the parser from
//! downstream logic; a real terminal swaps in callbacks that mutate
//! grid/cursor state, but those failure modes belong to grid/terminal
//! fuzz targets, not this one.

use libfuzzer_sys::fuzz_target;
use mars::parser::{Parser, ParserCallbacks};

struct Sink;
impl ParserCallbacks for Sink {
    fn print(&mut self, _ch: char) {}
    fn execute(&mut self, _byte: u8) {}
    fn esc_dispatch(&mut self, _intermediates: &[u8], _byte: u8) {}
    fn csi_dispatch(&mut self, _params: &[u16], _intermediates: &[u8], _byte: u8) {}
    fn osc_dispatch(&mut self, _data: &[u8]) {}
}

fuzz_target!(|data: &[u8]| {
    let mut parser = Parser::new();
    let mut sink = Sink;
    for &b in data {
        parser.advance(&mut sink, b);
    }
});
