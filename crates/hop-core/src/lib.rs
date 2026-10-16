//! Hop's OS-independent core: the control hand-off state machine, screen
//! layout and edge mapping. Sans-IO: events in, actions out, time passed in
//! explicitly. See docs/ARCHITECTURE.md ("Control hand-off").

#![forbid(unsafe_code)]
