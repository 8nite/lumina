//! Blocking permission/question broker (permission.asked flow).
//! Mirrors Go `permPending`/`qPending`: the runner publishes
//! `permission.asked` / `question.asked` over the SSE broker and waits here
//! until a client answers POST /api/v1/permissions|questions/{reqId}.
//!
//! The wait uses `block_in_place` + `blocking_recv` so the synchronous tool
//! layer can pause a turn without deadlocking the async daemon.

use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};

#[derive(Debug, Clone, Serialize)]
pub struct PendingPerm {
    pub req_id: String,
    pub session: String,
    pub tool: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingQuestion {
    pub req_id: String,
    pub session: String,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Question {
    pub text: String,
    pub options: Vec<String>,
}

#[derive(Clone, Default, Debug)]
pub struct ApprovalGate {
    inner: Option<Arc<Inner>>,
}

#[derive(Debug)]
struct Inner {
    broker: broadcast::Sender<serde_json::Value>,
    perms: Mutex<HashMap<String, (PendingPerm, oneshot::Sender<String>)>>,
    questions: Mutex<HashMap<String, (PendingQuestion, oneshot::Sender<Vec<String>>)>>,
    /// (session, tool, target-prefix) grants from "allow for session".
    grants: Mutex<HashSet<(String, String, String)>>,
}

impl ApprovalGate {
    pub fn new(broker: broadcast::Sender<serde_json::Value>) -> Self {
        Self { inner: Some(Arc::new(Inner {
            broker,
            perms: Mutex::new(HashMap::new()),
            questions: Mutex::new(HashMap::new()),
            grants: Mutex::new(HashSet::new()),
        })) }
    }

    fn inner(&self) -> Option<&Arc<Inner>> {
        self.inner.as_ref()
    }

/// Blocking receive with a deadline (polls in short slices so Ctrl+C in the
/// daemon console can still interrupt between slices).
fn wait_timeout<T>(mut rx: oneshot::Receiver<T>, max: Duration) -> Result<T, ()> {
    let start = std::time::Instant::now();
    loop {
        match rx.try_recv() {
            Ok(v) => return Ok(v),
            Err(oneshot::error::TryRecvError::Closed) => return Err(()),
            Err(oneshot::error::TryRecvError::Empty) => {
                if start.elapsed() >= max {
                    return Err(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

    pub fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    /// Ask to proceed. Returns true when allowed. Headless (no gate) callers
    /// never reach here — the tool layer errors before asking.
    pub fn ask(&self, session: &str, tool: &str, target: &str) -> Result<bool, String> {
        let inner = self.inner().ok_or_else(|| "approval gate not attached".to_string())?;
        {
            let grants = inner.grants.lock().unwrap();
            if grants.iter().any(|(s, t, p)| s == session && t == tool && target.starts_with(p)) {
                return Ok(true);
            }
        }
        let req_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        inner.perms.lock().unwrap().insert(
            req_id.clone(),
            (PendingPerm { req_id: req_id.clone(), session: session.into(), tool: tool.into(), target: target.into() }, tx),
        );
        let _ = inner.broker.send(serde_json::json!({
            "type": "permission.asked", "req_id": req_id,
            "session": session, "tool": tool, "target": target,
        }));
        let decision = Self::wait_timeout(rx, Duration::from_secs(600))
            .map_err(|_| "permission: timed out waiting for an answer".to_string())?;
        match decision.as_str() {
            "allow_once" => Ok(true),
            "allow_session" | "allow_always" => {
                inner.grants.lock().unwrap().insert((session.into(), tool.into(), target.into()));
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn ask_question(&self, session: &str, questions: Vec<Question>) -> Result<Vec<String>, String> {
        let inner = self.inner().ok_or_else(|| "question gate not attached".to_string())?;
        let req_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        inner.questions.lock().unwrap().insert(
            req_id.clone(),
            (PendingQuestion { req_id: req_id.clone(), session: session.into(), questions: questions.clone() }, tx),
        );
        let _ = inner.broker.send(serde_json::json!({
            "type": "question.asked", "req_id": req_id, "session": session,
            "questions": questions.iter().map(|q| serde_json::json!({"text": q.text, "options": q.options})).collect::<Vec<_>>(),
        }));
        tokio::task::block_in_place(|| {
            Self::wait_timeout(rx, Duration::from_secs(600))
                .map_err(|_| "question: timed out waiting for an answer".to_string())
        })
    }

    /// Resolve a pending permission (POST /api/v1/permissions/{reqId}).
    /// Returns false when the request id is unknown.
    pub fn resolve(&self, req_id: &str, decision: &str) -> bool {
        let Some(inner) = self.inner() else { return false };
        match inner.perms.lock().unwrap().remove(req_id) {
            Some((meta, tx)) => {
                let _ = inner.broker.send(serde_json::json!({
                    "type": "permission.resolved", "req_id": req_id,
                    "session": meta.session, "decision": decision,
                }));
                tx.send(decision.into()).is_ok()
            }
            None => false,
        }
    }

    pub fn answer(&self, req_id: &str, answers: Vec<String>) -> bool {
        let Some(inner) = self.inner() else { return false };
        match inner.questions.lock().unwrap().remove(req_id) {
            Some((meta, tx)) => {
                let _ = inner.broker.send(serde_json::json!({
                    "type": "question.resolved", "req_id": req_id, "session": meta.session,
                }));
                tx.send(answers).is_ok()
            }
            None => false,
        }
    }

    pub fn pending_for_session(&self, session: &str) -> (Option<PendingPerm>, Option<PendingQuestion>) {
        let Some(inner) = self.inner() else { return (None, None) };
        let perm = inner
            .perms
            .lock()
            .unwrap()
            .values()
            .find(|(m, _)| m.session == session)
            .map(|(m, _)| m.clone());
        let ques = inner
            .questions
            .lock()
            .unwrap()
            .values()
            .find(|(m, _)| m.session == session)
            .map(|(m, _)| m.clone());
        (perm, ques)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_resolve_roundtrip() {
        let (tx, _rx) = broadcast::channel(16);
        let gate = ApprovalGate::new(tx);
        let g2 = gate.clone();
        // Answer from another thread once the request is pending.
        let h = std::thread::spawn(move || {
            for _ in 0..100 {
                let (perm, _) = g2.pending_for_session("s1");
                if let Some(p) = perm {
                    assert_eq!(p.tool, "bash");
                    assert!(g2.resolve(&p.req_id, "allow_once"));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("no pending request appeared");
        });
        assert!(gate.ask("s1", "bash", "ls").unwrap());
        h.join().unwrap();
        // allow_session grants stick for the same target prefix.
        let (tx2, _rx2) = broadcast::channel(16);
        let gate2 = ApprovalGate::new(tx2);
        let g3 = gate2.clone();
        let h2 = std::thread::spawn(move || {
            for _ in 0..100 {
                let (perm, _) = g3.pending_for_session("s2");
                if let Some(p) = perm {
                    assert!(g3.resolve(&p.req_id, "allow_session"));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("no pending request appeared");
        });
        assert!(gate2.ask("s2", "edit", "a.rs").unwrap());
        h2.join().unwrap();
        // Second ask on the same prefix passes without a new request.
        assert!(gate2.ask("s2", "edit", "a.rs").unwrap());
    }
}
