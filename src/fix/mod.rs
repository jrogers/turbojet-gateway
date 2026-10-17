//! Conversion between FIX messages and the book's types, one module per FIX version. Pure: no
//! sessions and no clocks; the caller passes the time for TransactTime.

use turbojet::fields::{Code, CompactString, Decimal, FixEnum, UtcTimestamp, format_compact};
use turbojet::message::tags;
use turbojet::{Message, MessageReject};

use crate::book::{ClientId, Input, OrdType, Output, Status, VenueId};

pub mod fix42;
pub mod fix44;

/// A session's FIX version, from its BeginString(8): which module converts its messages. Parse
/// it once when the session is configured, so an unsupported version stops the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Fix42,
    Fix44,
}

/// A BeginString with no conversion: neither FIX.4.2 nor FIX.4.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedVersion(pub String);

impl std::fmt::Display for UnsupportedVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BeginString {:?} is not supported: use FIX.4.2 or FIX.4.4", self.0)
    }
}

impl std::error::Error for UnsupportedVersion {}

impl std::str::FromStr for Version {
    type Err = UnsupportedVersion;

    fn from_str(begin_string: &str) -> Result<Self, Self::Err> {
        match begin_string {
            "FIX.4.2" => Ok(Self::Fix42),
            "FIX.4.4" => Ok(Self::Fix44),
            _ => Err(UnsupportedVersion(begin_string.into())),
        }
    }
}

impl Version {
    /// BeginString(8).
    pub fn begin_string(self) -> &'static str {
        match self {
            Self::Fix42 => "FIX.4.2",
            Self::Fix44 => "FIX.4.4",
        }
    }

    /// The version's `client_input`.
    ///
    /// # Errors
    ///
    /// The reject to answer the client with.
    pub fn client_input(self, client: &ClientId, msg: &Message, redelivered: bool) -> Result<Input, MessageReject> {
        match self {
            Self::Fix42 => fix42::client_input(client, msg, redelivered),
            Self::Fix44 => fix44::client_input(client, msg, redelivered),
        }
    }

    /// The version's `client_message`.
    pub fn client_message(self, output: &Output, now: UtcTimestamp) -> Option<Message> {
        match self {
            Self::Fix42 => fix42::client_message(output, now),
            Self::Fix44 => fix44::client_message(output, now),
        }
    }

    /// The version's `venue_message`.
    pub fn venue_message(self, output: &Output, now: UtcTimestamp) -> Option<Message> {
        match self {
            Self::Fix42 => fix42::venue_message(output, now),
            Self::Fix44 => fix44::venue_message(output, now),
        }
    }

    /// The version's `venue_input`.
    ///
    /// # Errors
    ///
    /// The reject to answer the venue with.
    pub fn venue_input(self, venue: &VenueId, msg: &Message) -> Result<Option<Input>, MessageReject> {
        match self {
            Self::Fix42 => fix42::venue_input(venue, msg),
            Self::Fix44 => fix44::venue_input(venue, msg),
        }
    }
}

/// PossDupFlag(43) or PossResend(97) is set: the message may have been seen before, so the
/// book answers a known ClOrdID with the order's state rather than a duplicate reject.
pub fn possibly_resent(msg: &Message) -> bool {
    msg.flag(tags::POSS_DUP_FLAG) || msg.flag(tags::POSS_RESEND)
}

/// When a message was first sent: OrigSendingTime(122) on a resend, else SendingTime(52). `None`
/// if it has neither, or they don't parse (the session would have rejected that).
pub fn first_sent(msg: &Message) -> Option<chrono::DateTime<chrono::Utc>> {
    let time = |tag| msg.field::<UtcTimestamp>(tag).ok();
    time(tags::ORIG_SENDING_TIME).or_else(|| time(tags::SENDING_TIME)).map(Into::into)
}

// Helpers the versions share: nothing here touches a generated type.

/// OrderQty(38): optional in FIX 4.2 and 4.4 (CashOrderQty may stand in), but the book needs it.
fn quantity(qty: Option<Decimal>) -> Result<Decimal, MessageReject> {
    let qty = qty.ok_or(MessageReject::conditionally_required_field_missing(tags::ORDER_QTY))?;
    if qty <= Decimal::ZERO {
        return Err(MessageReject::value_incorrect(tags::ORDER_QTY, "OrderQty must be positive"));
    }
    Ok(qty)
}

/// A Limit order needs Price(44) and a Market order mustn't have one.
fn check_price(ord_type: OrdType, price: Option<Decimal>) -> Result<(), MessageReject> {
    match (ord_type, price) {
        (OrdType::Limit, None) => Err(MessageReject::conditionally_required_field_missing(tags::PRICE)),
        (OrdType::Market, Some(_)) => Err(MessageReject::value_incorrect(tags::PRICE, "A market order takes no Price")),
        _ => Ok(()),
    }
}

/// The reject for an enumerated value the book doesn't model, or a code the dictionary doesn't list.
fn unsupported(tag: u32, name: &str, code: &str) -> MessageReject {
    MessageReject::value_incorrect(tag, format!("{name} {code} is not supported"))
}

fn required(msg: &Message, tag: u32) -> Result<&str, MessageReject> {
    msg.get(tag).ok_or(MessageReject::required_tag_missing(tag))
}

/// OrdStatus Replaced says the replace was accepted, not where the order stands: that's in the
/// quantities.
fn replaced_status(cum_qty: Decimal, leaves_qty: Option<Decimal>) -> Status {
    match leaves_qty {
        _ if cum_qty <= Decimal::ZERO => Status::New,
        Some(leaves_qty) if leaves_qty <= Decimal::ZERO => Status::Filled,
        _ => Status::PartiallyFilled,
    }
}

/// OrdRejReason(103) or CxlRejReason(102) as the venue's number, whatever dictionary it's from.
fn reason_code(venue: &VenueId, downstream: &str, msg: &Message, tag: u32) -> Option<u32> {
    let code = msg.get(tag)?;
    let number = code.parse().ok();
    if number.is_none() {
        tracing::warn!(%venue, downstream, tag, code, "reason code isn't a number; dropped");
    }
    number
}

/// OrderID(37) is required on ExecutionReport and OrderCancelReject; "NONE" for no order.
fn order_id_out(order_id: Option<&CompactString>) -> CompactString {
    order_id.cloned().unwrap_or_else(|| "NONE".into())
}

/// The enum value for a numeric FIX code, if the enum has it.
fn venue_code<E: FixEnum>(code: u32) -> Code<E> {
    let code = format_compact!("{code}");
    E::from_code(&code).map_or_else(|| Code::Unknown(code.into()), Code::Known)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Book;
    use crate::router::{Router, Rule, Target};
    use turbojet::FixMessage;
    use turbojet_fix::{fix42 as v42, fix44 as v44};

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

    #[test]
    fn versions_parse_from_begin_string() {
        assert_eq!("FIX.4.2".parse(), Ok(Version::Fix42));
        assert_eq!("FIX.4.4".parse(), Ok(Version::Fix44));
        for unsupported in ["FIX.4.3", "FIXT.1.1", "fix.4.4", ""] {
            assert_eq!(unsupported.parse::<Version>(), Err(UnsupportedVersion(unsupported.into())));
        }
    }

    fn book() -> Book {
        Book::new(Router::new(vec![Rule {
            client: None,
            ex_destination: None,
            symbol: None,
            target: Target::Venue("NASDAQ".into()),
        }]))
    }

    fn run(book: &mut Book, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        book.handle(input, &mut out);
        out
    }

    fn time() -> UtcTimestamp {
        UtcTimestamp::from_timestamp(1_760_000_000, 0).unwrap()
    }

    /// A 4.2 client's order reaches a 4.4 venue, and the venue's fills come back in 4.2 terms.
    #[test]
    fn a_fix_42_client_trades_on_a_fix_44_venue() {
        let (client, venue): (Version, Version) = ("FIX.4.2".parse().unwrap(), "FIX.4.4".parse().unwrap());
        let mut book = book();
        let handl_inst = v42::HandlInst::AutomatedExecutionNoIntervention;
        let mut order = v42::NewOrderSingle::new("C1", handl_inst, "AAPL", v42::Side::Buy, time(), v42::OrdType::Limit);
        (order.order_qty, order.price) = (Some(100.into()), Some(10.into()));
        let input = client.client_input(&"FUNDA".into(), &order.to_message(), false).unwrap();
        let [output] = run(&mut book, input).try_into().unwrap();
        let sent: v44::NewOrderSingle = venue.venue_message(&output, time()).unwrap().parse().unwrap();
        assert_eq!((sent.cl_ord_id.as_str(), sent.symbol.as_deref()), ("1", Some("AAPL")));

        // LastQty, CumQty, LeavesQty.
        let fills = [
            (40, 40, 60, v44::OrdStatus::PartiallyFilled, v42::ExecType::PartialFill),
            (60, 100, 0, v44::OrdStatus::Filled, v42::ExecType::Fill),
        ];
        for (last_qty, cum_qty, leaves_qty, status, exec_type) in fills {
            let [last_qty, cum_qty, leaves_qty, px] = [last_qty, cum_qty, leaves_qty, 10].map(Decimal::from);
            let (trade, side) = (v44::ExecType::Trade, v44::Side::Buy);
            let mut fill = v44::ExecutionReport::new("V-9", "X1", trade, status, side, leaves_qty, cum_qty, px);
            fill.cl_ord_id = Some(sent.cl_ord_id.clone());
            (fill.last_qty, fill.last_px) = (Some(last_qty), Some(px));
            let input = venue.venue_input(&"NASDAQ".into(), &fill.to_message()).unwrap().unwrap();
            let [output] = run(&mut book, input).try_into().unwrap();
            let er: v42::ExecutionReport = client.client_message(&output, time()).unwrap().parse().unwrap();
            assert_eq!(
                (er.exec_trans_type, er.exec_type),
                (Code::Known(v42::ExecTransType::New), Code::Known(exec_type))
            );
            assert_eq!((er.cl_ord_id.as_deref(), er.order_id.as_str()), (Some("C1"), "1"));
            assert_eq!((er.last_shares, er.cum_qty, er.leaves_qty), (Some(last_qty), cum_qty, leaves_qty));
        }
    }

    /// A 4.4 client's order reaches a 4.2 venue with HandlInst, and the venue's PartialFill and
    /// Fill come back as 4.4 Trades.
    #[test]
    fn a_fix_44_client_trades_on_a_fix_42_venue() {
        let (client, venue): (Version, Version) = ("FIX.4.4".parse().unwrap(), "FIX.4.2".parse().unwrap());
        let mut book = book();
        let mut order = v44::NewOrderSingle::new("C1", v44::Side::Buy, time(), v44::OrdType::Limit);
        (order.symbol, order.order_qty, order.price) = (Some("AAPL".into()), Some(100.into()), Some(10.into()));
        let input = client.client_input(&"FUNDA".into(), &order.to_message(), false).unwrap();
        let [output] = run(&mut book, input).try_into().unwrap();
        let sent: v42::NewOrderSingle = venue.venue_message(&output, time()).unwrap().parse().unwrap();
        assert_eq!((sent.cl_ord_id.as_str(), sent.symbol.as_str()), ("1", "AAPL"));
        assert_eq!(sent.handl_inst, v42::HandlInst::AutomatedExecutionNoIntervention);

        // LastShares, CumQty, LeavesQty.
        let fills = [
            (40, 40, 60, v42::ExecType::PartialFill, v42::OrdStatus::PartiallyFilled, v44::OrdStatus::PartiallyFilled),
            (60, 100, 0, v42::ExecType::Fill, v42::OrdStatus::Filled, v44::OrdStatus::Filled),
        ];
        for (last_qty, cum_qty, leaves_qty, exec_type, status, client_status) in fills {
            let [last_qty, cum_qty, leaves_qty, px] = [last_qty, cum_qty, leaves_qty, 10].map(Decimal::from);
            let (new, side) = (v42::ExecTransType::New, v42::Side::Buy);
            let mut fill =
                v42::ExecutionReport::new("V-9", "X1", new, exec_type, status, "AAPL", side, leaves_qty, cum_qty, px);
            fill.cl_ord_id = Some(sent.cl_ord_id.clone());
            (fill.last_shares, fill.last_px) = (Some(last_qty), Some(px));
            let input = venue.venue_input(&"NASDAQ".into(), &fill.to_message()).unwrap().unwrap();
            let [output] = run(&mut book, input).try_into().unwrap();
            let er: v44::ExecutionReport = client.client_message(&output, time()).unwrap().parse().unwrap();
            assert_eq!((er.exec_type, er.ord_status), (Code::Known(v44::ExecType::Trade), Code::Known(client_status)));
            assert_eq!((er.cl_ord_id.as_deref(), er.order_id.as_str()), (Some("C1"), "1"));
            assert_eq!((er.last_qty, er.cum_qty, er.leaves_qty), (Some(last_qty), cum_qty, leaves_qty));
        }
    }
}
