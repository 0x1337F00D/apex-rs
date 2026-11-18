// SPDX-License-Identifier: GPL-2.0-only

#![allow(
    dead_code,
    reason = "the driver modules are compiled here only to be tested"
)]
//! Compiles the driver's hardware-independent modules for the host so they can be
//! tested with `cargo test`. The files are the exact sources the kernel module
//! builds; nothing here is a copy.

#[path = "../../driver/abi.rs"]
pub mod abi;
#[path = "../../driver/pagetable.rs"]
pub mod pagetable;
#[path = "../../driver/regs.rs"]
pub mod regs;
#[path = "../../driver/text.rs"]
pub mod text;
#[path = "../../driver/thermal.rs"]
pub mod thermal;

#[cfg(test)]
mod abi_tests;
#[cfg(test)]
mod pagetable_tests;
#[cfg(test)]
mod regs_tests;
#[cfg(test)]
mod text_tests;
#[cfg(test)]
mod thermal_tests;
