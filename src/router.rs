//! Routing rules.

use turbojet::fields::CompactString;

use crate::book::VenueId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Venue(VenueId),
    /// Reject the order, with this Text(58).
    Reject(CompactString),
}

#[derive(Debug, Clone, Default)]
pub struct Router;
