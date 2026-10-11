use chrono::{DateTime, Utc};
use turbojet::fields::CompactString;

use super::{
    Book, CancelKind, GatewayCancel, MAX_GATEWAY_CANCEL_TRIES, Order, OrderId, Origin, OrphanedReplace, Output,
    Pending, Scope, Sent, VenueId, limit_price,
};

impl Book {
    /// Whether a refused send has left a gateway cancel waiting since the last call: the edge's
    /// retry tick looks for waiting cancels only then.
    pub fn take_cancel_unsent(&mut self) -> bool {
        std::mem::take(&mut self.cancel_unsent)
    }

    pub(super) fn gateway_cancel_in_flight(&self, order: &Order) -> bool {
        self.outstanding_cancel(order).is_some_and(|sent| sent.origin == Origin::Gateway)
    }

    /// The order's latest cancel, while the venue hasn't answered it.
    fn outstanding_cancel(&self, order: &Order) -> Option<&Sent> {
        let sent = &self.downstream[order.cancel.as_ref()?];
        (!sent.answered).then_some(sent)
    }

    pub(super) fn cancel_all(&mut self, scope: &Scope, out: &mut Vec<Output>) {
        let mut ids: Vec<OrderId> = self
            .orders
            .values()
            .filter(|order| scope.covers(&order.client) && !order.status.is_done())
            .map(|order| order.id)
            .collect();
        // Map order is random; a fixed one keeps the venues' traffic and the tests repeatable.
        ids.sort_unstable_by_key(|id| id.0);
        for id in ids {
            self.cancel_order(id, out);
        }
    }

    /// Starts the gateway's own cancel for the order, the tries counted afresh: an operator asking
    /// again gets a stuck order another round. One in flight is left alone; one waits behind the
    /// client's cancel outstanding, going if the venue refuses that.
    pub(super) fn cancel_order(&mut self, id: OrderId, out: &mut Vec<Output>) {
        match self.start_gateway_cancel(id) {
            Some(false) => self.send_gateway_cancel(id, out),
            Some(true) => {
                tracing::info!(order = %self.orders[&id].name, "gateway cancel waits on the client's");
            }
            None => {}
        }
    }

    /// Starts the gateway's cancel for the order, unsent: `Some` with whether a client's cancel
    /// is outstanding, or `None` if the order is unknown, done, or has the gateway's in flight.
    fn start_gateway_cancel(&mut self, id: OrderId) -> Option<bool> {
        let Some(order) = self.orders.get(&id) else {
            tracing::warn!(order = id.0, "gateway cancel for an unknown order");
            return None;
        };
        if order.status.is_done() {
            tracing::debug!(order = %order.name, "gateway cancel for a done order");
            return None;
        }
        let outstanding = self.outstanding_cancel(order).map(|sent| sent.origin);
        if outstanding == Some(Origin::Gateway) {
            tracing::debug!(order = %order.name, "gateway cancel already in flight");
            return None;
        }
        let order = self.orders.get_mut(&id).expect("looked up above");
        self.touched.orders.insert(id);
        let orig_downstream = order.downstream.clone();
        order.gateway_cancel =
            Some(GatewayCancel { tries: 0, unsent: true, stuck: false, orig_downstream, refused_sends: 0 });
        Some(outstanding.is_some())
    }

    /// Sends the gateway's cancel for the order one more time.
    fn send_gateway_cancel(&mut self, id: OrderId, out: &mut Vec<Output>) {
        let sent = Sent {
            order: id,
            origin: Origin::Gateway,
            cl_ord_id: CompactString::default(),
            orig_cl_ord_id: None,
            kind: Some(CancelKind::Cancel),
            answered: false,
            sent_at: self.now,
        };
        let downstream = self.next_downstream(sent);
        let order = self.orders.get_mut(&id).expect("requests are for booked orders");
        self.touched.orders.insert(id);
        let gateway = order.gateway_cancel.as_mut().expect("the caller started the gateway's cancel");
        debug_assert!(gateway.tries < MAX_GATEWAY_CANCEL_TRIES && !gateway.stuck);
        gateway.tries += 1;
        gateway.unsent = false;
        gateway.orig_downstream = order.downstream.clone();
        tracing::info!(order = %order.name, %downstream, tries = gateway.tries, "gateway cancelling an order");
        out.push(Output::Cancel {
            venue: order.venue.clone(),
            downstream,
            orig_downstream: order.downstream.clone(),
            request: order.request.clone(),
            venue_order_id: order.venue_order_id.clone(),
        });
    }

    /// Sends the gateway's cancels for the venue that are waiting, with no cancel outstanding.
    pub(super) fn venue_up(&mut self, venue: &VenueId, out: &mut Vec<Output>) {
        let mut ids: Vec<OrderId> = self
            .orders
            .values()
            .filter(|order| order.venue == *venue && self.gateway_cancel_waiting(order))
            .map(|order| order.id)
            .collect();
        ids.sort_unstable_by_key(|id| id.0);
        for id in ids {
            self.send_gateway_cancel(id, out);
        }
    }

    /// See [`Input::Orphan`](super::Input::Orphan).
    pub(super) fn orphan(&mut self, venue: &VenueId, downstream: CompactString, replace: Option<OrphanedReplace>) {
        let Some((name, amendment)) =
            downstream.rsplit_once('-').and_then(|(name, n)| Some((name, n.parse::<u32>().ok()?)))
        else {
            return tracing::error!(%venue, %downstream, "orphaned request isn't a cancel's or replace's");
        };
        if self.downstream.contains_key(&downstream) || self.orphan_waiting(&downstream) {
            return tracing::debug!(%venue, %downstream, "orphaned request already recorded");
        }
        // ponytail: a walk of every order per orphan, at a start only; a map by name if many.
        let Some(order) = self.orders.values_mut().find(|order| order.name == name) else {
            return tracing::error!(%venue, %downstream, "venue holds a request the store lacks, for an order no longer held (done); nothing to cancel");
        };
        if order.venue != *venue {
            return tracing::error!(%venue, %downstream, order_venue = %order.venue, "orphaned request for another venue's order");
        }
        let id = order.id;
        self.touched.orders.insert(id);
        // Numbered past it, so no later request reuses its ID.
        order.amendments = order.amendments.max(amendment);
        // A done order has no replace pending: its replace can only be refused.
        let replace = replace.filter(|_| !order.status.is_done());
        let waits = order.pending.is_some();
        match replace {
            None => {
                tracing::error!(%venue, %downstream, order = %order.name, client = %order.client, symbol = %order.request.symbol, status = ?order.status, "venue holds a cancel the store lacks: cancelling its order");
                self.downstream.insert(downstream.clone(), gateway_sent(id, CancelKind::Cancel, self.now));
                self.touched.sent.insert(downstream);
                // Sent at `VenueUp`, or by the edge's retry tick if the venue is up already.
                if self.start_gateway_cancel(id).is_some() {
                    self.cancel_unsent = true;
                }
            }
            Some(OrphanedReplace { qty, price, ord_type, time_in_force }) => {
                tracing::error!(%venue, %downstream, order = %order.name, client = %order.client, symbol = %order.request.symbol, status = ?order.status, ?replace, "venue holds a replace the store lacks: pending on its order");
                let price = limit_price(ord_type, price);
                let risk_price = self.risk.risk_price(&order.request.symbol, price);
                // Set when it's made pending.
                let cl_ord_id = CompactString::default();
                let pending = Pending { downstream, cl_ord_id, qty, price, ord_type, time_in_force, risk_price };
                if waits {
                    // The book sent this replace only once the one pending was answered, so that
                    // answer, lost with it, is redelivered first: see `orphaned_replaces`.
                    tracing::info!(order = %order.name, pending = ?order.pending.as_ref().map(|p| &p.downstream), "orphaned replace waits for the pending one's answer");
                    self.orphaned_replaces.insert(id, (pending, None));
                } else {
                    self.make_orphan_pending(id, pending, None);
                }
            }
        }
    }

    /// Whether `downstream` is an orphaned replace waiting for the replace pending before it.
    pub fn orphan_waiting(&self, downstream: &str) -> bool {
        self.orphaned_replaces.values().any(|(pending, _)| pending.downstream == downstream)
    }

    /// Makes orphaned replaces pending whose orders' pending replaces have been answered: after
    /// every input, and a walk of none but at a start.
    pub(super) fn promote_orphaned_replaces(&mut self) {
        if self.orphaned_replaces.is_empty() {
            return;
        }
        let ready: Vec<OrderId> = self
            .orphaned_replaces
            .keys()
            .filter(|id| self.orders.get(id).is_none_or(|order| order.pending.is_none()))
            .copied()
            .collect();
        for id in ready {
            let (pending, adopted) = self.orphaned_replaces.remove(&id).expect("listed above");
            self.make_orphan_pending(id, pending, adopted);
        }
    }

    /// Records orphaned replace `pending` as the gateway's, or as the client's if its redelivered
    /// replace `adopted` it (naming that OrigClOrdID), pending on its order unless that's done
    /// (then only its answers map to the order).
    fn make_orphan_pending(&mut self, id: OrderId, mut pending: Pending, adopted: Option<CompactString>) {
        let downstream = pending.downstream.clone();
        let sent = match &adopted {
            Some(orig_cl_ord_id) => Sent {
                order: id,
                origin: Origin::Client,
                cl_ord_id: pending.cl_ord_id.clone(),
                orig_cl_ord_id: Some(orig_cl_ord_id.clone()),
                kind: Some(CancelKind::Replace),
                answered: false,
                sent_at: self.now,
            },
            None => gateway_sent(id, CancelKind::Replace, self.now),
        };
        self.downstream.insert(downstream.clone(), sent);
        self.touched.sent.insert(downstream);
        let Some(order) = self.orders.get_mut(&id).filter(|order| !order.status.is_done()) else {
            return tracing::info!(order = id.0, downstream = %pending.downstream, "orphaned replace's order ended first");
        };
        self.touched.orders.insert(id);
        // Accepted, it leaves the order's ClOrdID as it is, unless the client's request turns
        // out to be it (see `adopt_orphaned_replace`).
        if adopted.is_none() {
            pending.cl_ord_id = order.request.cl_ord_id.clone();
        }
        order.pending = Some(pending);
        self.risk.track(id, order.exposure());
    }

    /// The gateway's cancel is waiting to be sent, and nothing stands in its way.
    fn gateway_cancel_waiting(&self, order: &Order) -> bool {
        order.gateway_cancel.as_ref().is_some_and(|gateway| gateway.unsent) && self.outstanding_cancel(order).is_none()
    }

    /// After the venue refused a request for the order: the gateway's cancel that waited on a
    /// client's goes now.
    pub(super) fn resume_gateway_cancel(&mut self, id: OrderId, out: &mut Vec<Output>) {
        if self.gateway_cancel_waiting(&self.orders[&id]) {
            self.send_gateway_cancel(id, out);
        }
    }

    /// Sends the gateway's cancel again, or marks the order stuck once it has had its tries.
    fn retry_gateway_cancel(&mut self, id: OrderId, out: &mut Vec<Output>) {
        let order = self.orders.get_mut(&id).expect("requests are for booked orders");
        self.touched.orders.insert(id);
        let gateway = order.gateway_cancel.as_mut().expect("the caller checked for the gateway's cancel");
        if gateway.tries >= MAX_GATEWAY_CANCEL_TRIES {
            gateway.stuck = true;
            return tracing::error!(order = %order.name, tries = gateway.tries, "gateway gave up cancelling an order");
        }
        self.send_gateway_cancel(id, out);
    }

    /// Orders the gateway is cancelling itself, by OrderId: what the shutdown log and the console
    /// show. A stuck one needs someone to look.
    pub fn cancelling(&self) -> Vec<(OrderId, GatewayCancel)> {
        let mut cancelling: Vec<_> =
            self.orders.values().filter_map(|order| Some((order.id, order.gateway_cancel.clone()?))).collect();
        cancelling.sort_unstable_by_key(|(id, _)| id.0);
        cancelling
    }

    /// Whether every cancel the gateway sent itself is answered or given up (stuck).
    pub fn cancels_settled(&self) -> bool {
        !self.orders.values().any(|order| order.gateway_cancel.as_ref().is_some_and(|cancel| !cancel.stuck))
    }

    /// The venue refused the gateway's own cancel: the client never hears of it. Sent again,
    /// unless the venue says it's too late or doesn't know the order, which another try won't
    /// change; an order that's live here all the same is stuck.
    pub(super) fn gateway_cancel_refused(
        &mut self,
        id: OrderId,
        downstream: &CompactString,
        reason: Option<u32>,
        text: Option<CompactString>,
        out: &mut Vec<Output>,
    ) {
        let order = self.orders.get_mut(&id).expect("downstream ClOrdIDs point at booked orders");
        self.touched.orders.insert(id);
        let text = text.unwrap_or_default();
        // The refusal's status made the order done, which ends the gateway's cancel.
        let Some(gateway) = order.gateway_cancel.as_mut() else {
            return tracing::info!(order = %order.name, %downstream, ?reason, %text, "venue refused the gateway's cancel of a done order");
        };
        tracing::error!(order = %order.name, %downstream, ?reason, %text, tries = gateway.tries, "venue refused the gateway's cancel");
        // CxlRejReason 0 is too late, 1 unknown order: unless a replace accepted since gave the
        // order a new downstream ClOrdID, which the venue may yet know.
        if matches!(reason, Some(0 | 1)) && order.downstream == gateway.orig_downstream {
            gateway.stuck = true;
            return tracing::error!(order = %order.name, "gateway gave up cancelling an order the venue says is done or unknown");
        }
        self.retry_gateway_cancel(id, out);
    }
}

/// A request the gateway itself sent for order `id`.
fn gateway_sent(id: OrderId, kind: CancelKind, sent_at: DateTime<Utc>) -> Sent {
    Sent {
        order: id,
        origin: Origin::Gateway,
        cl_ord_id: CompactString::default(),
        orig_cl_ord_id: None,
        kind: Some(kind),
        answered: false,
        sent_at,
    }
}
