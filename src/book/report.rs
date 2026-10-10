use turbojet::fields::{CompactString, Decimal, format_compact};

use super::{
    Book, CancelKind, ClientReport, ExecType, Order, Origin, Output, Refused, RejectReason, Sent, Status, VenueId,
    VenueReport,
};

impl Book {
    pub(super) fn venue_report(&mut self, venue: &VenueId, mut report: VenueReport, out: &mut Vec<Output>) {
        let Some(sent) = self.downstream.get(&report.downstream).cloned() else {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "execution report for an unknown order");
        };
        // Rejected under a cancel's or replace's ClOrdID is about that request, not the order.
        // Some venues refuse an amendment with ExecType=8 and the order's live OrdStatus: that
        // status stands, as an OrderCancelReject's would.
        let rejected =
            report.status.is_some_and(|status| status == Status::Rejected || report.exec_type == ExecType::Rejected);
        if rejected && sent.kind.is_some() {
            let status = report.status.filter(|&status| status != Status::Rejected);
            // An ExecutionReport has no CxlRejReason: the refusal's reason is Other.
            let refused = Refused { status, text: report.text, venue_order_id: report.venue_order_id, reason: None };
            let id = sent.order;
            self.refuse_request(venue, &report.downstream, sent, refused, out);
            return self.resume_gateway_cancel(id, out);
        }
        let Sent { order: id, origin, cl_ord_id, orig_cl_ord_id, kind, answered } = sent;
        let order = self.orders.get_mut(&id).expect("downstream ClOrdIDs point at booked orders");
        if order.venue != *venue {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "execution report for another venue's order");
        }
        if stale(order, answered, &report) {
            // A restatement is the venue changing the order: losing one needs a look.
            if report.exec_type == ExecType::Restated {
                return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "restatement dropped during a pending request");
            }
            return tracing::debug!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "stale execution report dropped");
        }
        // A fill whose status is behind (PartiallyFilled sent before a cancel the order now
        // awaits, say) applies keeping the order's status, as a report without one would: it
        // then accepts and settles nothing.
        if behind(order, answered, &report) {
            report.status = None;
        }
        let pending_here = order.pending.as_ref().is_some_and(|pending| pending.downstream == report.downstream);
        // Any answer under the replace's ClOrdID but these means the venue took it.
        let accepted =
            pending_here && !rejected && report.status.is_some_and(|status| status != Status::PendingReplace);
        // A client's cancel's reports and a pending replace's (up to the one accepting it) answer
        // that request; any other, the gateway's cancel's too, is about the order, unsolicited.
        let answers_request = match kind {
            Some(CancelKind::Cancel) => origin == Origin::Client,
            // An orphaned replace's acks are the gateway's, as its cancel's are.
            Some(CancelKind::Replace) => pending_here && origin == Origin::Client,
            None => false,
        };
        let qty = match &order.pending {
            Some(pending) if accepted => pending.qty,
            _ => order.request.qty,
        };
        if report.cum_qty > qty {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, cum_qty = %report.cum_qty, "overfill");
        }
        self.touched.orders.insert(id);
        order.record_venue_order_id(report.venue_order_id.take(), accepted);
        if accepted {
            // The downstream the replace supersedes is settled like any other: its late reports
            // without a fill are resends.
            let superseded = order.apply_replace();
            self.downstream.get_mut(&superseded).expect("an order's downstream is recorded").answered = true;
            self.touched.sent.insert(superseded);
        }
        if accepted || (kind == Some(CancelKind::Cancel) && report.status == Some(Status::Canceled)) {
            self.downstream.get_mut(&report.downstream).expect("looked up above").answered = true;
            self.touched.sent.insert(report.downstream.clone());
        }
        // What the report filled: more than its LastQty when it carries a fill the gateway
        // missed (a Status report without one, or a fill after a gap).
        let rise = report.cum_qty - order.cum_qty;
        debug_assert!(rise >= Decimal::ZERO, "stale drops a lower CumQty");
        let price = fill_price(order, &report, rise);
        order.cum_qty = report.cum_qty;
        order.avg_px = report.avg_px;
        // `None` keeps the status, through `set_status` so a fill makes a New order PartiallyFilled.
        order.set_status(report.status.unwrap_or(order.status));
        if rise > Decimal::ZERO {
            let reference = (report.last_qty > Decimal::ZERO).then_some(report.last_px);
            self.risk.fill(&order.client, &order.request.symbol, rise, price, reference);
            self.touched.fill(order);
        }
        self.risk.track(id, order.exposure());
        // The gateway's cancel's acks aren't the client's: after a refusal it would be left
        // showing PendingCancel. Fills and the end are.
        if origin == Origin::Gateway && rise == Decimal::ZERO && !order.status.is_done() {
            return tracing::debug!(%venue, downstream = %report.downstream, "gateway cancel's ack not forwarded");
        }
        let answers = answers_request.then_some((cl_ord_id, orig_cl_ord_id));
        let client_report = order.client_report(answers, report, rejected, (rise, price));
        out.push(Output::Report { client: order.client.clone(), report: client_report });
    }
}

impl Order {
    /// A report of where the order stands, answering the client's `cl_ord_id`, under the order's
    /// next ExecID.
    pub(super) fn report(&mut self, cl_ord_id: &CompactString, exec_type: ExecType) -> ClientReport {
        // Status answers to redeliveries and pass-through venue reports aren't bounded by the
        // ClOrdID limits, so a peer drives this count; at u64 it can't reach the top. Saturating
        // anyway, so no input can panic the book.
        self.exec_seq = self.exec_seq.saturating_add(1);
        ClientReport {
            exec_type,
            order_id: Some(self.name.clone()),
            cl_ord_id: cl_ord_id.clone(),
            orig_cl_ord_id: None,
            request: self.request.clone(),
            status: self.status,
            reject: None,
            last_qty: Decimal::ZERO,
            last_px: Decimal::ZERO,
            cum_qty: self.cum_qty,
            leaves_qty: self.leaves_qty(),
            avg_px: self.avg_px,
            exec_id: format_compact!("{}.{}", self.name, self.exec_seq),
            text: None,
        }
    }

    /// The client's report of the venue's `report`, which `filled` the order's CumQty up by a
    /// quantity at a price: under the ClOrdIDs of the request it `answers`, if any, else under
    /// the order's current ClOrdID.
    fn client_report(
        &mut self,
        answers: Option<(CompactString, Option<CompactString>)>,
        report: VenueReport,
        rejected: bool,
        filled: (Decimal, Decimal),
    ) -> ClientReport {
        let (last_qty, last_px) = filled;
        // Clients count fills by ExecType (FIX 4.2 has nothing else), so every fill is a Trade,
        // and its LastQty is the whole rise in CumQty, so a client's LastQtys add up to it.
        let exec_type = if last_qty > Decimal::ZERO { ExecType::Trade } else { report.exec_type };
        let mut client_report = match answers {
            Some((cl_ord_id, orig_cl_ord_id)) => {
                let mut client_report = self.report(&cl_ord_id, exec_type);
                client_report.orig_cl_ord_id = orig_cl_ord_id;
                client_report
            }
            None => self.report(&self.request.cl_ord_id.clone(), exec_type),
        };
        client_report.last_qty = last_qty;
        client_report.last_px = last_px;
        client_report.text = report.text;
        if rejected {
            client_report.reject = report.ord_rej_reason.map(RejectReason::Venue);
        }
        client_report
    }

    /// Where the order stands, answering a client asking by `asked`: a status request or a
    /// redelivered new order. Reports carry the order's latest ClOrdID; after a replace, OrigClOrdID
    /// names the one the client asked about.
    pub(super) fn status_report(&mut self, asked: CompactString) -> ClientReport {
        let mut report = self.report(&self.request.cl_ord_id.clone(), ExecType::Status);
        if report.cl_ord_id != asked {
            report.orig_cl_ord_id = Some(asked);
        }
        report
    }
}

/// A report the book has already moved past, such as a venue resend after a reconnect: applying it
/// would roll the order back. `answered` is whether the venue has settled the request the report
/// comes under. Dropped:
///
/// - anything for a done order (busts and corrections come later);
/// - a CumQty below the order's, or a fill at the order's CumQty (a resent fill);
/// - a report not raising CumQty whose status is [`behind`] the order's.
///
/// A report raising CumQty always applies, its fill reported by LastQty(32) or not: its status
/// is what may be behind.
pub(super) fn stale(order: &Order, answered: bool, report: &VenueReport) -> bool {
    order.status.is_done()
        || report.cum_qty < order.cum_qty
        || (report.cum_qty == order.cum_qty && (report.last_qty > Decimal::ZERO || behind(order, answered, report)))
}

/// Whether the report's status is behind the order's, so would roll it back:
///
/// - (a) `settled`: under a settled request, unless it's the order's current downstream, where
///   only the accepted replace's own PendingReplace is a resend (the order's later reports come
///   under it too). A done status is exempt: it can't roll anything back, and dropping it would
///   leave an order live here that the venue has ended;
/// - (b) `behind_new`: PendingNew once the order is past it;
/// - (c) `behind_pending`: New or PartiallyFilled while the order is PendingCancel or
///   PendingReplace, unless it accepts the pending replace: the venue refuses a request with an
///   OrderCancelReject, which comes another way.
///
/// A pending state may follow PartiallyFilled, so status alone doesn't order reports.
fn behind(order: &Order, answered: bool, report: &VenueReport) -> bool {
    let current = report.downstream == order.downstream;
    // A report keeping the order's status is never done, PendingNew or New.
    let status = report.status;
    let settled =
        answered && !status.is_some_and(Status::is_done) && (!current || status == Some(Status::PendingReplace));
    let behind_new = status == Some(Status::PendingNew) && order.status != Status::PendingNew;
    let accepts_pending = order.pending.as_ref().is_some_and(|pending| pending.downstream == report.downstream);
    let behind_pending = matches!(status, Some(Status::New | Status::PartiallyFilled))
        && matches!(order.status, Status::PendingCancel | Status::PendingReplace)
        && !accepts_pending;
    settled || behind_new || behind_pending
}

/// Decimal places an implied LastPx is rounded to: a division can run to 28 digits, too many to
/// put on the wire, and the price is only as exact as the venue's AvgPx rounding anyway.
const IMPLIED_PX_DP: u32 = 8;

/// The price of `rise`, what `report` takes the order's CumQty up by: its LastPx when that's
/// the report's own fill, else what the change in AvgPx implies for the rise (a fill the gateway
/// missed, approximate by the venue's AvgPx rounding), or AvgPx itself where that can't be
/// worked out or isn't positive.
fn fill_price(order: &Order, report: &VenueReport, rise: Decimal) -> Decimal {
    if rise == report.last_qty && report.last_px > Decimal::ZERO {
        return report.last_px;
    }
    let old = order.cum_qty.checked_mul(order.avg_px);
    let new = report.cum_qty.checked_mul(report.avg_px);
    new.zip(old)
        .and_then(|(new, old)| new.checked_sub(old))
        .and_then(|missed| missed.checked_div(rise))
        .map(|price| price.round_dp(IMPLIED_PX_DP))
        .filter(|&price| price > Decimal::ZERO)
        .unwrap_or(report.avg_px)
}
