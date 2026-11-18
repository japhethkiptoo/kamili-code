//! Non-interactive channel for CI: questions get no answer, approvals follow a
//! fixed policy (reject unless `--yes`).

use async_trait::async_trait;
use kamili::{ApprovalRequest, Channel, Decision, Event, Question};

use crate::terminal::print_event;

pub struct CiChannel {
    pub approve_all: bool,
}

#[async_trait]
impl Channel for CiChannel {
    fn event(&self, event: Event) {
        print_event(&event);
    }

    async fn ask(&self, _: &Question) -> Option<String> {
        None
    }

    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        let decision = if self.approve_all {
            Decision::Approve
        } else {
            Decision::Reject
        };
        eprintln!("approval {:?}: {decision:?} (policy)", request.kind);
        decision
    }
}
