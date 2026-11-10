//! Reserve a result until semantic admission succeeds. Rejection leaves it queued.

use super::{Back, Delivery, Due, MAX_WAITING, Table, settle};
use std::path::Path;
use std::sync::Arc;

pub(crate) struct ResultDelivery {
    table: Arc<Table>,
    execution: u64,
    run: u64,
    index: usize,
    dropped: usize,
    active: bool,
    by: Option<message::MessageId>,
    pub due: Due,
}

impl Table {
    pub(crate) fn claim_result(
        self: &Arc<Self>,
        root: &Path,
        session: &str,
    ) -> Option<ResultDelivery> {
        let mut inner = self.lock();
        let (at, index) = inner
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.root == root
                    && !item.delivering
                    && Delivery::Session(Some(session)).accepts(item)
            })
            .filter_map(|(at, item)| {
                item.waiting
                    .iter()
                    .position(|back| matches!(back, Back::Result(_)))
                    .map(|index| (at, index))
            })
            .min_by_key(|(at, _)| inner.items[*at].delivery_error.is_some())?;
        let item = &mut inner.items[at];
        let Back::Result(due) = &item.waiting[index] else {
            unreachable!("result selection")
        };
        let mut due = due.clone();
        let dropped = item.dropped;
        if dropped > 0 {
            due.line.push_str(&format!(
                "\n\n({dropped} later lines were dropped: more than {MAX_WAITING} waited at once.)"
            ));
        }
        item.delivering = true;
        Some(ResultDelivery {
            table: self.clone(),
            execution: item.id,
            run: item.run,
            index,
            dropped,
            active: true,
            by: item.owner.as_ref().and_then(|owner| owner.request.clone()),
            due,
        })
    }
}

impl ResultDelivery {
    pub fn message(&self, session: message::SessionId) -> message::WireMessage {
        message::WireMessage {
            id: self.due.event.clone(),
            session,
            by: self.by.clone(),
            at: self.due.at,
            kind: message::WireKind::Request(message::Request::ToModel(
                message::InputKind::Automatic,
            )),
            body: vec![
                message::Block::Text(self.due.line.clone()),
                message::Block::Data(serde_json::json!({
                    "source":format!("background:{}:{}",self.execution,self.run),
                    "reason":self.due.note,
                    "event":self.due.event,
                    "completion":self.due.completion,
                })),
            ],
        }
    }

    /// Report a changed refusal once. The same queued result can be retried later.
    pub fn refused(&self, why: &str) -> bool {
        let mut inner = self.table.lock();
        let Some(at) = inner.mine(self.execution, self.run) else {
            return false;
        };
        let item = &mut inner.items[at];
        if item.delivery_error.as_deref() == Some(why) {
            return false;
        }
        item.delivery_error = Some(why.into());
        tracing::warn!(target:"pi::background",execution=self.execution,run=self.run,%why,"result admission deferred");
        true
    }

    /// A stopped or restarted producer invalidates its outstanding reservation.
    pub fn commit(mut self) -> bool {
        self.active = false;
        let mut inner = self.table.lock();
        let Some(at) = inner.mine(self.execution, self.run) else {
            return false;
        };
        let item = &mut inner.items[at];
        if !item.delivering || !matches!(item.waiting.get(self.index), Some(Back::Result(_))) {
            item.delivering = false;
            return false;
        }
        item.waiting.remove(self.index);
        item.dropped -= self.dropped;
        item.delivery_error = None;
        item.delivering = false;
        settle(&mut inner.items, at);
        tracing::info!(target:"pi::background",execution=self.execution,run=self.run,kind="result",input=self.due.event.as_str(),"admitted");
        true
    }
}

impl Drop for ResultDelivery {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut inner = self.table.lock();
        if let Some(at) = inner.mine(self.execution, self.run) {
            inner.items[at].delivering = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::background::Commands;
    use ::execution::background::{Invocation, Owner, RunHandle, RunSink, Scope};

    fn source(table: &Arc<Table>) -> Arc<dyn RunHandle> {
        Commands::new(table.clone()).start(
            Scope {
                root: "/checkout".into(),
                owner: Some(Owner {
                    session: "session".into(),
                    request: None,
                }),
            },
            Invocation::shell("source"),
            Default::default(),
        )
    }

    #[test]
    fn reservation_is_exclusive_and_drop_leaves_the_original_result_queued() {
        let table = Arc::new(Table::default());
        let execution = source(&table);
        execution.channel().result("first".into());
        execution.channel().result("second".into());
        let root = Path::new("/checkout");
        assert!(table.claim_result(root, "other-session").is_none());
        let first = table.claim_result(root, "session").unwrap();
        let event = first.due.event.clone();
        assert!(table.claim_result(root, "session").is_none());
        assert!(table.take_back(root).is_none());
        drop(first);
        let first = table.claim_result(root, "session").unwrap();
        assert_eq!(first.due.event, event);
        assert!(first.commit());
        let second = table.claim_result(root, "session").unwrap();
        assert_ne!(second.due.event, event);
        assert!(second.due.line.contains("second"));
        assert!(second.commit());
        assert!(table.claim_result(root, "session").is_none());
    }

    #[test]
    fn a_deferred_producer_does_not_hold_back_other_producers() {
        let table = Arc::new(Table::default());
        let first = source(&table);
        first.channel().result("deferred".into());
        let root = Path::new("/checkout");
        let held = table.claim_result(root, "session").unwrap();
        held.refused("invalid payload");
        drop(held);
        let second = source(&table);
        second.channel().result("ready".into());
        let ready = table.claim_result(root, "session").unwrap();
        assert_eq!(ready.execution, second.id());
        assert!(ready.commit());
        let retry = table.claim_result(root, "session").unwrap();
        assert_eq!(retry.execution, first.id());
    }

    #[test]
    fn deferred_result_does_not_block_an_explicit_host_command() {
        let table = Arc::new(Table::default());
        let execution = source(&table);
        execution.channel().result("deferred".into());
        let root = Path::new("/checkout");
        let delivery = table.claim_result(root, "session").unwrap();
        assert!(delivery.refused("queue full"));
        drop(delivery);
        assert!(table.next_due(root, Some("session")).is_none());
        execution
            .channel()
            .input("host control".into(), String::new());
        assert!(table.next_due(root, Some("session")).is_some());
        let Some((_, Back::Input { text, .. })) = table.take_back(root) else {
            panic!()
        };
        assert_eq!(text, "host control");
        assert!(table.next_due(root, Some("session")).is_none());
        assert!(
            table
                .claim_result(root, "session")
                .unwrap()
                .due
                .line
                .contains("deferred")
        );
    }

    #[test]
    fn stopping_the_producer_invalidates_its_reserved_result() {
        let table = Arc::new(Table::default());
        let execution = source(&table);
        execution.channel().result("stale".into());
        let root = Path::new("/checkout");
        let delivery = table.claim_result(root, "session").unwrap();
        table.stop(root, execution.id()).unwrap();
        assert!(!delivery.commit());
        assert!(table.claim_result(root, "session").is_none());
    }

    #[test]
    fn overflow_notice_is_retained_on_rejection_and_consumed_once_on_commit() {
        let table = Arc::new(Table::default());
        let execution = source(&table);
        for _ in 0..MAX_WAITING + 1 {
            execution.channel().result("output".into());
        }
        let root = Path::new("/checkout");
        let first = table.claim_result(root, "session").unwrap();
        assert!(first.due.line.contains("later lines were dropped"));
        drop(first);
        let retry = table.claim_result(root, "session").unwrap();
        assert!(retry.due.line.contains("later lines were dropped"));
        assert!(retry.commit());
        let next = table.claim_result(root, "session").unwrap();
        assert!(!next.due.line.contains("later lines were dropped"));
    }
}
