//! Backends that drive the conversation. Wizard's only backend is the
//! NexAU code agent, reached through a long-lived Python bridge subprocess
//! that speaks NDJSON over stdio (see [`nexau`] and `backend/nexau_bridge.py`).

pub mod nexau;
