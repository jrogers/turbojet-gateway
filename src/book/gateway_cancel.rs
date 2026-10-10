use turbojet::fields::CompactString;

use super::{
    Book, CancelKind, GatewayCancel, MAX_GATEWAY_CANCEL_TRIES, Order, OrderId, Origin, Output, Scope, Sent, VenueId,
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
        let Some(order) = self.orders.get(&id) else {
            return tracing::warn!(order = id.0, "gateway cancel for an unknown order");
        };
        if order.status.is_done() {
            return tracing::debug!(order = %order.name, "gateway cancel for a done order");
        }
        let outstanding = self.outstanding_cancel(order).map(|sent| sent.origin);
        if outstanding == Some(Origin::Gateway) {
            return tracing::debug!(order = %order.name, "gateway cancel already in flight");
        }
        let order = self.orders.get_mut(&id).expect("looked up above");
        self.touched.orders.insert(id);
        let orig_downstream = order.downstream.clone();
        order.gateway_cancel =
            Some(GatewayCancel { tries: 0, unsent: true, stuck: false, orig_downstream, refused_sends: 0 });
        if outstanding.is_some() {
            return tracing::info!(order = %order.name, "gateway cancel waits on the client's");
        }
        self.send_gateway_cancel(id, out);
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
