//! Bounded caching layer with RAM (and later disk) tiers, plus single-flight
//! request coalescing. Implemented in Phase 4; Phase 1 defines the boundary.

#![forbid(unsafe_code)]
