use turbojet::fields::{CompactString, Decimal, format_compact};

use super::{
    Book, CancelKind, CancelRejectReason, ClientId, ExecType, OrdType, Order, OrderId, OrderRequest, Origin, Output,
    Pending, RejectReason, Sent, Side, Status, TimeInForce, VenueId, limit_price,
};
use crate::risk::{self, Proposed, RiskReject};

impl Book {
    /// A cancel goes to the venue even while PendingNew, under the next numbered downstream ClOrdID.
    /// One is refused while the gateway's own cancel is in flight: the order is going already, and
    /// the client hears when it's gone. A stuck one, or one waiting for the venue's session to take it, lets
    /// the client's through, which may fare better.
    pub(super) fn cancel(&mut self, amend: Amend, named: (Side, CompactString), out: &mut Vec<Output>) {
        let refuse = |book: &Self, order: &Order| {
            book.gateway_cancel_in_flight(order)
                .then_some((CancelRejectReason::AlreadyPending, "the gateway is cancelling the order"))
        };
        let Some((id, amend)) = self.amendable(amend, named, refuse, out) else { return };
        let (downstream, order) = self.number(id, amend);
        out.push(Output::Cancel {
            venue: order.venue.clone(),
            downstream,
            orig_downstream: order.downstream.clone(),
            request: order.request.clone(),
            venue_order_id: order.venue_order_id.clone(),
        });
    }

    /// A replace goes to the venue like a cancel, one at a time; the order keeps its terms until
    /// the venue accepts it (see `venue_report`). Refused, like a cancel, while the gateway's own
    /// cancel is in flight; for a blocked client, if it raises the quantity; and if its new terms
    /// break a risk limit (see [`risk::check`]).
    pub(super) fn replace(&mut self, amend: Amend, named: (Side, CompactString), terms: Terms, out: &mut Vec<Output>) {
        if amend.redelivered && self.adopt_orphaned_replace(&amend, &named, &terms, out) {
            return;
        }
        let Terms { qty, price, ord_type, time_in_force } = terms;
        let refuse = |book: &Self, order: &Order| {
            if book.gateway_cancel_in_flight(order) {
                Some((CancelRejectReason::AlreadyPending, "the gateway is cancelling the order"))
            } else if order.pending.is_some() {
                Some((CancelRejectReason::AlreadyPending, "replace pending"))
            } else if qty < order.cum_qty {
                Some((CancelRejectReason::Other, "quantity below filled quantity"))
            } else if qty > order.request.qty && book.is_blocked(&order.client) {
                Some((CancelRejectReason::Other, "blocked: a replace may not raise the quantity"))
            } else {
                let price = limit_price(ord_type.unwrap_or(order.request.ord_type), price);
                book.check_replace(order, qty, price).err().map(|reject| (CancelRejectReason::Other, reject.text()))
            }
        };
        let Some((id, amend)) = self.amendable(amend, named, refuse, out) else { return };
        let cl_ord_id = amend.cl_ord_id.clone();
        let (downstream, order) = self.number(id, amend);
        let ord_type = ord_type.unwrap_or(order.request.ord_type);
        let time_in_force = time_in_force.or(order.request.time_in_force);
        debug_assert!(
            (ord_type == OrdType::Limit) == price.is_some(),
            "the edge refuses a Limit replace without a price and a Market one with a price"
        );
        // A market order has no price: the release fallback should the contract slip.
        let price = limit_price(ord_type, price);
        out.push(Output::Replace {
            venue: order.venue.clone(),
            downstream: downstream.clone(),
            orig_downstream: order.downstream.clone(),
            request: OrderRequest { qty, price, ord_type, time_in_force, ..order.request.clone() },
            venue_order_id: order.venue_order_id.clone(),
        });
        let symbol = order.request.symbol.clone();
        let risk_price = self.risk.risk_price(&symbol, price);
        let order = self.orders.get_mut(&id).expect("looked up above");
        self.touched.orders.insert(id);
        order.pending = Some(Pending { downstream, cl_ord_id, qty, price, ord_type, time_in_force, risk_price });
        self.risk.track(id, order.exposure());
    }

    /// Takes a client's redelivered replace as the orphaned replace (see `Input::Orphan`) it was
    /// sent as before a restart, if the order's pending replace, or the one it last accepted, is
    /// an orphan with the same terms: the request becomes the client's, so the venue's answers
    /// answer it. Accepted already, it's answered as Replaced now. `false` if there's none.
    fn adopt_orphaned_replace(
        &mut self,
        amend: &Amend,
        named: &(Side, CompactString),
        terms: &Terms,
        out: &mut Vec<Output>,
    ) -> bool {
        let Some(&id) = self.index.get(&(amend.client.clone(), amend.orig_cl_ord_id.clone())) else { return false };
        if self.index.contains_key(&(amend.client.clone(), amend.cl_ord_id.clone())) {
            return false;
        }
        let order = &self.orders[&id];
        if (order.request.side, &order.request.symbol) != (named.0, &named.1) {
            return false;
        }
        let ord_type = terms.ord_type.unwrap_or(order.request.ord_type);
        let wanted = (
            terms.qty,
            limit_price(ord_type, terms.price),
            ord_type,
            terms.time_in_force.or(order.request.time_in_force),
        );
        let orphan = |downstream: &CompactString| {
            self.downstream
                .get(downstream)
                .is_some_and(|sent| sent.origin == Origin::Gateway && sent.kind == Some(CancelKind::Replace))
        };
        let accepted = match &order.pending {
            Some(p) if orphan(&p.downstream) && (p.qty, p.price, p.ord_type, p.time_in_force) == wanted => false,
            None if orphan(&order.downstream) => {
                let r = &order.request;
                if (r.qty, r.price, r.ord_type, r.time_in_force) != wanted {
                    return false;
                }
                true
            }
            _ => return false,
        };
        let downstream = if accepted {
            order.downstream.clone()
        } else {
            order.pending.as_ref().expect("matched above").downstream.clone()
        };
        tracing::info!(order = %order.name, %downstream, cl_ord_id = %amend.cl_ord_id, accepted, "a redelivered replace is the orphaned one");
        self.remember(&amend.client, amend.cl_ord_id.clone(), id);
        let sent = self.downstream.get_mut(&downstream).expect("matched above");
        (sent.origin, sent.cl_ord_id, sent.orig_cl_ord_id) =
            (Origin::Client, amend.cl_ord_id.clone(), Some(amend.orig_cl_ord_id.clone()));
        self.touched.sent.insert(downstream);
        let order = self.orders.get_mut(&id).expect("looked up above");
        self.touched.orders.insert(id);
        if !accepted {
            order.pending.as_mut().expect("matched above").cl_ord_id = amend.cl_ord_id.clone();
            return true;
        }
        order.request.cl_ord_id = amend.cl_ord_id.clone();
        let mut report = order.report(&amend.cl_ord_id, ExecType::Replaced);
        report.orig_cl_ord_id = Some(amend.orig_cl_ord_id.clone());
        out.push(Output::Report { client: amend.client.clone(), report });
        true
    }

    /// A replace of `order` to `qty` at `price` (`None`: Market) against the client's limits. One
    /// that adds no risk, at the same price (or Market again) and no more quantity, passes, so a
    /// client can always cut an order the limits would refuse now.
    fn check_replace(&self, order: &Order, qty: Decimal, price: Option<Decimal>) -> Result<(), RiskReject> {
        if price == limit_price(order.request.ord_type, order.request.price) && qty <= order.request.qty {
            return Ok(());
        }
        let proposed = Proposed {
            client: &order.client,
            symbol: &order.request.symbol,
            venue: &order.venue,
            side: order.request.side,
            qty,
            leaves: qty - order.cum_qty,
            price,
            replaces: Some(order.id),
        };
        risk::check(self.limits.limits(order.client.comp_id()), &proposed, &self.risk)
    }

    /// The live order a cancel or replace names, or `None` with an OrderCancelReject refusing it
    /// pushed, or the order's state answering a resend of a request already taken. `refuse` adds
    /// the request's own checks after the shared ones, the last of which is that the request
    /// `named` the order's side and symbol.
    fn amendable(
        &mut self,
        amend: Amend,
        named: (Side, CompactString),
        refuse: impl Fn(&Self, &Order) -> Option<(CancelRejectReason, &'static str)>,
        out: &mut Vec<Output>,
    ) -> Option<(OrderId, Amend)> {
        let id = self.index.get(&(amend.client.clone(), amend.orig_cl_ord_id.clone())).copied();
        // Before `remember`, which asserts the ClOrdID is new.
        let used_for = self.index.get(&(amend.client.clone(), amend.cl_ord_id.clone())).copied();
        if let Some(id) = id.filter(|&id| amend.redelivered && used_for == Some(id)) {
            // Side and symbol aren't checked again: the ClOrdID already maps to this order.
            let order = self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders");
            self.touched.orders.insert(id);
            // It answers that request, under its own ClOrdIDs, as the venue's reports for it do.
            let mut report = order.report(&amend.cl_ord_id, ExecType::Status);
            report.orig_cl_ord_id = Some(amend.orig_cl_ord_id);
            out.push(Output::Report { client: amend.client, report });
            return None;
        }
        let order = id.map(|id| self.orders.get(&id).expect("indexed ClOrdIDs point at booked orders"));
        let duplicate = used_for.is_some();
        let refusal = match order {
            _ if duplicate => Some((CancelRejectReason::Duplicate, "duplicate ClOrdID")),
            None => Some((CancelRejectReason::UnknownOrder, "unknown order")),
            Some(order) if order.status.is_done() => Some((CancelRejectReason::TooLate, "order is done")),
            Some(_)
                if self
                    .used
                    .get(&amend.client)
                    .is_some_and(|&used| used >= self.max_cl_ord_ids + self.amend_reserve) =>
            {
                Some((CancelRejectReason::Other, "too many requests today"))
            }
            Some(order) if (order.request.side, &order.request.symbol) != (named.0, &named.1) => {
                Some((CancelRejectReason::Other, "side or symbol does not match"))
            }
            Some(order) => refuse(self, order),
        };
        if let Some((reason, text)) = refusal {
            out.push(amend.refuse(order, reason, text.into()));
            return None;
        }
        Some((order.expect("an unknown order is refused").id, amend))
    }

    /// Sends `amend` under the order's next numbered downstream ClOrdID, remembering what it was.
    fn number(&mut self, id: OrderId, amend: Amend) -> (CompactString, &mut Order) {
        let sent = Sent {
            order: id,
            origin: Origin::Client,
            cl_ord_id: amend.cl_ord_id.clone(),
            orig_cl_ord_id: Some(amend.orig_cl_ord_id),
            kind: Some(amend.kind),
            answered: false,
        };
        let downstream = self.next_downstream(sent);
        self.remember(&amend.client, amend.cl_ord_id, id);
        (downstream, self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders"))
    }

    /// The order's next numbered downstream ClOrdID, recording `sent` under it.
    pub(super) fn next_downstream(&mut self, sent: Sent) -> CompactString {
        let order = self.orders.get_mut(&sent.order).expect("requests are for booked orders");
        self.touched.orders.insert(sent.order);
        order.amendments += 1;
        let downstream = format_compact!("{}-{}", order.name, order.amendments);
        if sent.kind == Some(CancelKind::Cancel) {
            order.cancel = Some(downstream.clone());
        }
        let previous = self.downstream.insert(downstream.clone(), sent);
        debug_assert!(previous.is_none(), "downstream ClOrdID {downstream} reused");
        self.touched.sent.insert(downstream.clone());
        downstream
    }

    /// The venue refused a cancel or replace we sent under `downstream`, with the order's status
    /// as the venue now gives it.
    pub(super) fn venue_cancel_reject(
        &mut self,
        venue: &VenueId,
        downstream: &CompactString,
        refused: Refused,
        out: &mut Vec<Output>,
    ) {
        let Some(sent) = self.downstream.get(downstream).cloned() else {
            return tracing::warn!(%venue, %downstream, "cancel reject for an unknown request");
        };
        let id = sent.order;
        self.refuse_request(venue, downstream, sent, refused, out);
        self.resume_gateway_cancel(id, out);
    }

    /// The venue's session couldn't take what we sent under `downstream`, so the venue never saw
    /// it. A cancel or replace is refused, leaving the order's status as it is; an original order
    /// still PendingNew is rejected.
    pub(super) fn send_failed(
        &mut self,
        venue: &VenueId,
        downstream: &CompactString,
        text: CompactString,
        out: &mut Vec<Output>,
    ) {
        let Some(sent) = self.downstream.get(downstream).cloned() else {
            return tracing::warn!(%venue, %downstream, "send failed for an unknown request");
        };
        let order = self.orders.get_mut(&sent.order).expect("downstream ClOrdIDs point at booked orders");
        self.touched.orders.insert(sent.order);
        debug_assert_eq!(order.venue, *venue, "the edge reports a failed send to the order's venue");
        if order.venue != *venue {
            return tracing::warn!(%venue, %downstream, "send failed for another venue's order");
        }
        // The gateway's own cancel waits until the venue's session can take it (on logon or the
        // edge's retry tick): nothing to send now, which the edge relies on.
        if sent.origin == Origin::Gateway {
            if sent.answered {
                return tracing::debug!(%venue, %downstream, "send failed for an answered request; dropped");
            }
            self.downstream.get_mut(downstream).expect("looked up above").answered = true;
            self.touched.sent.insert(downstream.clone());
            // Done since, the order needs no more cancelling.
            let Some(gateway) = &mut order.gateway_cancel else {
                return tracing::debug!(%venue, %downstream, "send failed for a gateway cancel of a done order");
            };
            debug_assert!(gateway.tries > 0 && !gateway.unsent && !gateway.stuck);
            gateway.tries -= 1;
            gateway.unsent = true;
            gateway.refused_sends = gateway.refused_sends.saturating_add(1);
            self.cancel_unsent = true;
            if gateway.refused_sends > 1 {
                return tracing::debug!(%venue, %downstream, %text, refused = gateway.refused_sends, "gateway cancel not sent again");
            }
            return tracing::warn!(%venue, %downstream, %text, "gateway cancel not sent; again when the venue's session can take it");
        }
        // A cancel or replace the venue never saw is refused even on a done order: nobody else
        // will answer it. `refuse_request` drops one already answered.
        if sent.kind.is_some() {
            let refused = Refused { status: Some(order.status), text: Some(text), venue_order_id: None, reason: None };
            return self.refuse_request(venue, downstream, sent, refused, out);
        }
        // A venue's report means the original got there; done is final, as in `stale`.
        if sent.answered || order.status != Status::PendingNew {
            return tracing::debug!(%venue, %downstream, "send failed for an answered order; dropped");
        }
        self.downstream.get_mut(downstream).expect("looked up above").answered = true;
        self.touched.sent.insert(downstream.clone());
        order.set_status(Status::Rejected);
        self.risk.track(order.id, order.exposure());
        let mut report = order.report(&order.request.cl_ord_id.clone(), ExecType::Rejected);
        report.reject = Some(RejectReason::VenueUnavailable);
        report.text = Some(text);
        out.push(Output::Report { client: order.client.clone(), report });
    }

    /// See [`Input::Unsent`](super::Input::Unsent).
    pub(super) fn unsent(&mut self, venue: &VenueId, downstream: CompactString, out: &mut Vec<Output>) {
        let Some(sent) = self.downstream.get(&downstream) else {
            return tracing::warn!(%venue, %downstream, "an unknown request to send again");
        };
        let order = &self.orders[&sent.order];
        let resendable = !sent.answered && !order.status.is_done() && order.venue == *venue;
        let output = match sent.kind {
            _ if !resendable => None,
            None => (order.status == Status::PendingNew).then(|| Output::NewOrder {
                venue: venue.clone(),
                downstream: downstream.clone(),
                request: order.request.clone(),
            }),
            Some(CancelKind::Cancel) => Some(Output::Cancel {
                venue: venue.clone(),
                downstream: downstream.clone(),
                orig_downstream: order.downstream.clone(),
                request: order.request.clone(),
                venue_order_id: order.venue_order_id.clone(),
            }),
            Some(CancelKind::Replace) => order.pending.as_ref().filter(|p| p.downstream == downstream).map(|p| {
                let request = OrderRequest {
                    qty: p.qty,
                    price: p.price,
                    ord_type: p.ord_type,
                    time_in_force: p.time_in_force,
                    ..order.request.clone()
                };
                Output::Replace {
                    venue: venue.clone(),
                    downstream: downstream.clone(),
                    orig_downstream: order.downstream.clone(),
                    request,
                    venue_order_id: order.venue_order_id.clone(),
                }
            }),
        };
        match output {
            Some(output) => {
                tracing::warn!(%venue, %downstream, "sending again a request the venue's session never stored");
                out.push(output);
            }
            None => self.send_failed(venue, &downstream, "not sent before the gateway restarted".into(), out),
        }
    }

    /// The venue refused the cancel or replace `sent` under `downstream`, by OrderCancelReject or
    /// by a Rejected ExecutionReport: tell the client, under the ClOrdIDs of its own request.
    /// Anything that doesn't name such a request is dropped.
    pub(super) fn refuse_request(
        &mut self,
        venue: &VenueId,
        downstream: &CompactString,
        sent: Sent,
        refused: Refused,
        out: &mut Vec<Output>,
    ) {
        let Refused { status, text, venue_order_id, reason } = refused;
        let Some(response_to) = sent.kind else {
            return tracing::warn!(%venue, %downstream, "request rejected naming an order, not a cancel or replace");
        };
        let order = self.orders.get_mut(&sent.order).expect("downstream ClOrdIDs point at booked orders");
        self.touched.orders.insert(sent.order);
        if order.venue != *venue {
            return tracing::warn!(%venue, %downstream, "request rejected for another venue's order");
        }
        if sent.answered {
            return tracing::debug!(%venue, %downstream, "rejection of an answered request dropped");
        }
        self.downstream.get_mut(downstream).expect("looked up by the caller").answered = true;
        self.touched.sent.insert(downstream.clone());
        // The venue named its order while refusing the request: as good as any report's.
        order.record_venue_order_id(venue_order_id, false);
        // Only the replace it names: an older one's late reject leaves a newer one pending.
        if order.pending.as_ref().is_some_and(|pending| pending.downstream == *downstream) {
            order.pending = None;
        }
        match status {
            // Done is final, as in `stale`.
            _ if order.status.is_done() => {}
            Some(status) => order.set_status(status),
            // Without the venue's word, the refused request no longer holds the order pending.
            None if matches!(order.status, Status::PendingCancel | Status::PendingReplace) => {
                order.set_status(if order.cum_qty > Decimal::ZERO { Status::PartiallyFilled } else { Status::New });
            }
            None => {}
        }
        self.risk.track(order.id, order.exposure());
        if sent.origin == Origin::Gateway {
            // Not the gateway's latest cancel (an orphaned request, see `Input::Orphan`, or a
            // cancel since superseded): its refusal leaves the gateway's cancel as it is.
            if order.cancel.as_ref() != Some(downstream) {
                return tracing::warn!(%venue, %downstream, ?reason, ?text, "venue refused a gateway request other than its latest cancel; the order's cancel goes on");
            }
            return self.gateway_cancel_refused(sent.order, downstream, reason, text, out);
        }
        let orig_cl_ord_id = sent.orig_cl_ord_id.expect("a client's cancel or replace names an order");
        let client = order.client.clone();
        let amend = Amend { client, cl_ord_id: sent.cl_ord_id, orig_cl_ord_id, kind: response_to, redelivered: false };
        let text = text.unwrap_or_else(|| "rejected by the venue".into());
        let reason = reason.map_or(CancelRejectReason::Other, CancelRejectReason::Venue);
        out.push(amend.refuse(Some(&*order), reason, text));
    }
}

/// A venue's refusal of a cancel or replace. `status` is the order's status as the venue gives
/// it, if it does.
pub(super) struct Refused {
    pub(super) status: Option<Status>,
    pub(super) text: Option<CompactString>,
    pub(super) venue_order_id: Option<CompactString>,
    /// The venue's CxlRejReason; `None` reports Other.
    pub(super) reason: Option<u32>,
}

/// A client's cancel or replace request.
pub(super) struct Amend {
    pub(super) client: ClientId,
    pub(super) cl_ord_id: CompactString,
    pub(super) orig_cl_ord_id: CompactString,
    pub(super) kind: CancelKind,
    /// Possibly a resend, as `Input::Cancel` says.
    pub(super) redelivered: bool,
}

/// A replace's new terms, as `Input::Replace` gives them.
pub(super) struct Terms {
    pub(super) qty: Decimal,
    pub(super) price: Option<Decimal>,
    pub(super) ord_type: Option<OrdType>,
    pub(super) time_in_force: Option<TimeInForce>,
}

impl Amend {
    /// An OrderCancelReject refusing it, naming the order when there is one.
    fn refuse(self, order: Option<&Order>, reason: CancelRejectReason, text: CompactString) -> Output {
        Output::CancelReject {
            client: self.client,
            order_id: order.map(|order| order.name.clone()),
            cl_ord_id: self.cl_ord_id,
            orig_cl_ord_id: self.orig_cl_ord_id,
            response_to: self.kind,
            status: order.map_or(Status::Rejected, |order| order.status),
            reason,
            text,
        }
    }
}
