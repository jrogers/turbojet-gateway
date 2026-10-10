//! Conversion between FIX messages and the book's types, one module per FIX version. Pure: no
//! sessions and no clocks; the caller passes the time for TransactTime.

use turbojet::Message;
use turbojet::message::tags;

pub mod fix42;
pub mod fix44;

/// PossDupFlag(43) or PossResend(97) is set: the message may have been seen before, so the
/// book answers a known ClOrdID with the order's state rather than a duplicate reject.
pub fn possibly_resent(msg: &Message) -> bool {
    msg.flag(tags::POSS_DUP_FLAG) || msg.flag(tags::POSS_RESEND)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn possibly_resent_reads_poss_dup_and_poss_resend() {
        for (tag, value, expected) in [
            (tags::POSS_DUP_FLAG, "Y", true),
            (tags::POSS_RESEND, "Y", true),
            (tags::POSS_DUP_FLAG, "N", false),
            (tags::POSS_RESEND, "N", false),
        ] {
            let mut msg = Message::default();
            msg.push(tags::MSG_TYPE, "D");
            msg.push(tag, value);
            assert_eq!(possibly_resent(&msg), expected, "{tag}={value}");
        }
        assert!(!possibly_resent(&Message::default()));
    }
}
