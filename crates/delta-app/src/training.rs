//! M4/M5 K-line Replay Training Application Service, Truncated Data Port, Hidden Mode Scrubbing,
//! Deterministic Recovery/Replay, Notes/Archive, and Scoped Read-Only AI Tools (`S-37`..`S-55`).

mod dto;
mod host_api;
mod lifecycle;
mod notes_ai;
mod recovery;
mod scrub;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

pub use dto::*;
pub use scrub::{
    bar_label_for_one_based, hidden_display_symbol_for_category, is_run_identity_masked,
    record_to_core_fee_profile,
};

pub(crate) use scrub::{
    changed_orders, map_sim_err, map_sim_err_scrubbed, map_storage_err, sim_fill_to_record,
    sim_order_to_record,
};

/// Replayed state, per-seq recomputed hashes and the undo replayer at the latest boundary.
pub(crate) type RecoveredSim = (
    delta_core::SimState,
    Vec<(i64, String)>,
    delta_core::SimReplayer,
);
