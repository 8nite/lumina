//! Blocking permission/question broker (permission.asked flow).
//! Mirrors Go `permPending`/`qPending`: the runner publishes
//! `permission.asked` / `question.asked` over the SSE broker and waits here
//! until a client answers POST /api/v1/permissions|questions/{reqId}.
//!
//! The wait uses `block_in_place` + `blocking_recv` so the synchronous tool
//! layer can pause a turn without deadlocking the async daemon.

use serde::{Deserialize, Serialize};
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

/// One option in a user question.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuestionOption {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// One user question: `{question, header, options: [{label, description}],
/// multiple}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Clone, Default, Debug)]
pub struct ApprovalGate {
    inner: Option<Arc<Inner>>,
}

#[derive(Debug)]
struct Inner {
    broker: broadcast::Sender<serde_json::Value>,
    perms: Mutex<PermMap>,
    questions: Mutex<QuesMap>,
    /// (session, tool, target-prefix) grants from "allow for session".
    grants: Mutex<HashSet<(String, String, String)>>,
}

type PermMap = HashMap<String, (PendingPerm, oneshot::Sender<String>)>;
type QuesMap = HashMap<String, (PendingQuestion, oneshot::Sender<Vec<Vec<String>>>)>;

/// Blocking-receive outcome: answered, peer dismissed, or deadline passed.
enum WaitErr {
    Closed, // peer dismissed the request (or went away)
    Timeout,
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
fn wait_timeout<T>(mut rx: oneshot::Receiver<T>, max: Duration) -> Result<T, WaitErr> {
    let start = std::time::Instant::now();
    loop {
        match rx.try_recv() {
            Ok(v) => return Ok(v),
            Err(oneshot::error::TryRecvError::Closed) => return Err(WaitErr::Closed),
            Err(oneshot::error::TryRecvError::Empty) => {
                if start.elapsed() >= max {
                    return Err(WaitErr::Timeout);
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
        let _ = inner.broker.send(crate::server::ev(
            "permission.asked",
            session,
            serde_json::json!({"request_id": req_id, "tool": tool, "target": target}),
        ));
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

    pub fn ask_question(&self, session: &str, questions: Vec<Question>) -> Result<Vec<Vec<String>>, String> {
        let inner = self.inner().ok_or_else(|| "question gate not attached".to_string())?;
        let req_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        inner.questions.lock().unwrap().insert(
            req_id.clone(),
            (PendingQuestion { req_id: req_id.clone(), session: session.into(), questions: questions.clone() }, tx),
        );
        let _ = inner.broker.send(crate::server::ev(
            "question.asked",
            session,
            serde_json::json!({"request_id": req_id, "questions": questions.iter().map(|q| serde_json::json!({"question": q.question, "header": q.header, "options": q.options, "multiple": q.multiple})).collect::<Vec<_>>() }),
        ));
        tokio::task::block_in_place(|| {
            Self::wait_timeout(rx, Duration::from_secs(600)).map_err(|e| match e {
                WaitErr::Closed => "question dismissed by user".to_string(),
                WaitErr::Timeout => "question: timed out waiting for an answer".to_string(),
            })
        })
    }

    /// Dismiss a pending question without answers (POST dismissed:true).
    /// Surfaces as an error on the waiting turn. Returns false when unknown.
    pub fn dismiss(&self, req_id: &str) -> bool {
        let Some(inner) = self.inner() else { return false };
        match inner.questions.lock().unwrap().remove(req_id) {
            Some((meta, _tx)) => {
                // Drop the sender: the waiter sees Closed → dismissed error.
                let _ = inner.broker.send(crate::server::ev(
                    "question.resolved",
                    &meta.session,
                    serde_json::json!({"request_id": req_id, "dismissed": true}),
                ));
                true
            }
            None => false,
        }
    }

    /// Resolve a pending permission (POST /api/v1/permissions/{reqId}).
    /// Returns false when the request id is unknown.
    pub fn resolve(&self, req_id: &str, decision: &str) -> bool {
        let Some(inner) = self.inner() else { return false };
        match inner.perms.lock().unwrap().remove(req_id) {
            Some((meta, tx)) => {
                let _ = inner.broker.send(crate::server::ev(
                    "permission.resolved",
                    &meta.session,
                    serde_json::json!({"request_id": req_id, "decision": decision}),
                ));
                tx.send(decision.into()).is_ok()
            }
            None => false,
        }
    }

    pub fn answer(&self, req_id: &str, answers: Vec<Vec<String>>) -> bool {
        let Some(inner) = self.inner() else { return false };
        match inner.questions.lock().unwrap().remove(req_id) {
            Some((meta, tx)) => {
                let _ = inner.broker.send(crate::server::ev(
                    "question.resolved",
                    &meta.session,
                    serde_json::json!({"request_id": req_id}),
                ));
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

    #[tokio::test(flavor = "multi_thread")]
    async fn dismiss_question_surfaces_error() {
        use tokio::sync::broadcast;
        let (tx, _rx) = broadcast::channel(16);
        let gate = ApprovalGate::new(tx);
        let g2 = gate.clone();
        // Dismiss from another thread once the request is pending.
        let h = std::thread::spawn(move || {
            for _ in 0..100 {
                let (_, ques) = g2.pending_for_session("s9");
                if let Some(q) = ques {
                    assert!(g2.dismiss(&q.req_id));
                    assert!(!g2.dismiss(&q.req_id)); // second dismiss: unknown
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("no pending question appeared");
        });
        let err = gate
            .ask_question("s9", vec![crate::approve::Question { question: "pick?".into(), header: String::new(), options: vec![crate::approve::QuestionOption { label: "a".into(), description: String::new() }], multiple: false }])
            .unwrap_err();
        assert_eq!(err, "question dismissed by user", "{err}");
        h.join().unwrap();
    }
}
