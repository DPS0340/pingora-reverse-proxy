//! Shutdown coordination placeholder.
//!
//! Task 8 startup must install crash reporting first, then call
//! `route_table::install_route_mutation_panic_hook_at_startup` and retain that
//! hook unchanged for the process lifetime. Shutdown must stop management
//! accepts before calling the terminal `RouteRegistry::drain_mutations`, then
//! surface timeout, detached diagnostic, and overflow fields before runtime
//! termination.
