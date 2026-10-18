//! Pure domain: command vocabulary and the poll-loop state machine.

pub mod command;
pub mod core;

pub use core::{
    Clock,
    Core,
    CoreTargets,
    CoreTiming,
    StepOutcome,
};

pub use command::{
    Command,
    Rgb,
};
