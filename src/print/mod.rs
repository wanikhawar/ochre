//! Printing, from Print Studio: page ranges, copies and collation, reverse
//! order, n-up, booklets, guided manual two-sided printing and every option
//! the printer's driver offers. Jobs go to CUPS as print-ready PDFs.

pub mod config;
pub mod cups;
pub mod geometry;
pub mod pdfout;
pub mod pipeline;
pub mod ppd;
pub mod render;
pub mod testpdf;
#[cfg(test)]
mod tests;
