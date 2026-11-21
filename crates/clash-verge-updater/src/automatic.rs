//! Local policy used by clients of the updater. The daemon never trusts it for authorization.
use crate::{
    audit::{Record, Source},
    manifest::Kind,
    state::Outcome as TxnOutcome,
    version,
};
use serde::{Deserialize, Serialize};

pub const DAY: i64 = 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub version: String,
    pub kind: Kind,
    pub sha256: String,
    pub test: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub identity: Identity,
    pub acquisition: String,
    pub test_mode: bool,
    pub signatures: Vec<String>,
    #[serde(default)]
    pub downloaded: bool,
    pub first_seen: i64,
    pub confirmed: Option<i64>,
    pub failures: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub identity: Identity,
    pub acquisition: String,
    pub started: i64,
    pub txn: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastResult {
    pub acquisition: String,
    pub result: String,
    pub reason: String,
    pub time: i64,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct State {
    pub candidate: Option<Candidate>,
    pub attempt: Option<Attempt>,
    pub last: Option<LastResult>,
    // Request ordinals are local to the running GUI.
    #[serde(skip)]
    pub applied_check: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Eligibility {
    Waiting(i64),
    Unconfirmed,
    Ready,
    Failed,
    Installed,
}

impl Candidate {
    pub const fn deadline(&self) -> i64 {
        self.first_seen.saturating_add(if self.test_mode { 0 } else { DAY })
    }

    pub fn eligibility(&self, now: i64, installed: Option<&str>) -> Eligibility {
        if installed.is_some_and(|v| version::compare(&self.identity.version, v).is_ok_and(|order| !order.is_gt())) {
            return Eligibility::Installed;
        }
        if self.failures >= 3 {
            return Eligibility::Failed;
        }
        if now < self.deadline() {
            return Eligibility::Waiting(self.deadline());
        }
        if self
            .confirmed
            .is_some_and(|t| t >= self.deadline() && t <= now && now.saturating_sub(t) <= 3 * DAY)
        {
            Eligibility::Ready
        } else {
            Eligibility::Unconfirmed
        }
    }

    pub fn attempt(&self, started: i64) -> Attempt {
        Attempt {
            identity: self.identity.clone(),
            acquisition: self.acquisition.clone(),
            started,
            txn: None,
        }
    }
}

impl State {
    pub fn authorize_staged(&mut self, attempt: &Attempt, installed: &str, enabled: bool, now: i64) -> bool {
        let Some(own) = self
            .attempt
            .as_mut()
            .filter(|a| a.acquisition == attempt.acquisition && a.txn.is_none())
        else {
            return false;
        };
        own.txn.clone_from(&attempt.txn);
        enabled
            && self.candidate.as_ref().is_some_and(|c| {
                c.acquisition == attempt.acquisition
                    && c.identity == attempt.identity
                    && c.eligibility(now, Some(installed)) == Eligibility::Ready
            })
    }

    pub fn component_unavailable(&mut self, acquisition: &str, reason: String, now: i64) {
        if self.candidate.as_ref().is_some_and(|c| c.acquisition == acquisition) {
            self.last = Some(LastResult {
                acquisition: acquisition.into(),
                result: "unavailable".into(),
                reason,
                time: now,
            });
        }
    }

    pub fn download_target(&mut self, acquisition: &str, test_mode: bool) -> Option<&mut Candidate> {
        self.candidate
            .as_mut()
            .filter(|c| c.acquisition == acquisition && c.test_mode == test_mode)
    }

    pub fn withdraw(&mut self) {
        self.candidate = None;
        self.last = None;
    }

    pub fn normalize(&mut self, now: i64, test_mode: bool) {
        if let Some(attempt) = &mut self.attempt {
            attempt.identity.sha256.make_ascii_lowercase();
        }
        if let Some(candidate) = &mut self.candidate {
            candidate.identity.sha256.make_ascii_lowercase();
            if candidate.test_mode != test_mode {
                self.withdraw();
            } else if candidate.first_seen > now || candidate.confirmed.is_some_and(|t| t > now) {
                candidate.first_seen = now;
                candidate.confirmed = None;
                if candidate.failures < 3 {
                    self.last = None;
                }
            }
        }
    }

    /// Only successful checks reach this point. A new acquisition has to survive the download
    /// before its bytes can be consumed. A later withdrawal invalidates its acquisition ID.
    pub fn observe(
        &mut self,
        ordinal: u64,
        started: i64,
        now: i64,
        test_mode: bool,
        offer: Option<(Identity, Vec<String>)>,
        acquisition: Option<String>,
    ) -> bool {
        if ordinal <= self.applied_check {
            return false;
        }
        self.applied_check = ordinal;
        self.normalize(now, test_mode);
        let Some((identity, signatures)) = offer.filter(|(offer, _)| !offer.test || test_mode) else {
            self.withdraw();
            return true;
        };
        if let Some(candidate) = &mut self.candidate
            && candidate.identity == identity
        {
            if acquisition.is_none() {
                return true;
            }
            candidate.signatures = signatures;
            if started >= candidate.deadline() && started <= now {
                candidate.confirmed = Some(now);
            }
        } else {
            self.withdraw();
            let Some(acquisition) = acquisition else { return true };
            self.candidate = Some(Candidate {
                identity,
                signatures,
                acquisition,
                test_mode,
                downloaded: false,
                first_seen: now,
                confirmed: None,
                failures: 0,
            });
        }
        true
    }
}

#[derive(Debug)]
pub enum Settlement {
    Interrupted,
    Active(Kind),
    Unavailable,
    Applied,
    Failed(String),
    RolledBack(String),
    Rejected(String),
    NeedsRepair,
    Superseded,
}

pub fn terminal(outcome: TxnOutcome, error: Option<String>) -> Settlement {
    match outcome {
        TxnOutcome::Applied => Settlement::Applied,
        TxnOutcome::RolledBack => Settlement::RolledBack(error.unwrap_or_else(|| "rolled back".into())),
        TxnOutcome::Failed => Settlement::Failed(error.unwrap_or_else(|| "update failed".into())),
        TxnOutcome::NeedsRepair => Settlement::NeedsRepair,
        TxnOutcome::Superseded | TxnOutcome::Abandoned => Settlement::Superseded,
    }
}

pub fn audit_terminal(id: &str, records: Vec<Record>) -> Option<Settlement> {
    records
        .into_iter()
        .filter(|r| r.txn.as_deref() == Some(id) && matches!(r.source, Source::Actual))
        .find_map(|r| {
            if r.event == "staged" {
                return Some(Settlement::Interrupted);
            }
            let outcome = serde_json::from_value(serde_json::Value::String(r.event)).ok()?;
            Some(terminal(outcome, r.error))
        })
}

/// Returns the failure to report after the state has been persisted.
pub fn settle(state: &mut State, observation: Settlement, now: i64) -> Option<String> {
    if matches!(observation, Settlement::Active(_) | Settlement::Unavailable) {
        return None;
    }
    let attempt = state.attempt.take()?;
    state
        .candidate
        .as_ref()
        .filter(|c| c.acquisition == attempt.acquisition && c.identity == attempt.identity)?;
    let (result, reason) = match observation {
        Settlement::Applied => {
            state.withdraw();
            ("applied", None)
        }
        Settlement::Failed(reason) => ("failed", Some(reason)),
        Settlement::RolledBack(reason) => ("rolled_back", Some(reason)),
        Settlement::Rejected(reason) => ("rejected", Some(reason)),
        Settlement::NeedsRepair => ("needs_repair", None),
        Settlement::Superseded => ("superseded", None),
        Settlement::Interrupted => ("interrupted", None),
        Settlement::Active(_) | Settlement::Unavailable => unreachable!(),
    };
    if reason.is_some()
        && let Some(candidate) = &mut state.candidate
    {
        candidate.failures = candidate.failures.saturating_add(1);
    }
    state.last = Some(LastResult {
        acquisition: attempt.acquisition,
        result: result.into(),
        reason: reason.clone().unwrap_or_default(),
        time: now,
    });
    reason
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn offer() -> Identity {
        Identity {
            version: "2.0.0".into(),
            kind: Kind::Patch,
            sha256: "digest".into(),
            test: false,
        }
    }

    fn checked(s: &mut State, ordinal: u64, started: i64, now: i64, mode: bool, offer: Option<Identity>) -> bool {
        s.observe(
            ordinal,
            started,
            now,
            mode,
            offer.map(|i| (i, vec!["signature".into()])),
            Some(format!("id-{ordinal}")),
        )
    }

    fn initial() -> State {
        let mut state = State::default();
        checked(&mut state, 1, 1, 10, false, Some(offer()));
        state
    }

    fn eligibility(s: &State, now: i64) -> Eligibility {
        s.candidate.as_ref().unwrap().eligibility(now, Some("1.0.0"))
    }

    #[test]
    fn confirmation_must_start_after_wait_and_remain_fresh() {
        let mut s = initial();
        let due = DAY + 10;
        assert_eq!(eligibility(&s, due - 1), Eligibility::Waiting(due));
        checked(&mut s, 2, due - 1, due + 1, false, Some(offer()));
        assert_eq!(eligibility(&s, due + 1), Eligibility::Unconfirmed);
        checked(&mut s, 3, due, due + 2, false, Some(offer()));
        assert_eq!(eligibility(&s, due + 2 + 3 * DAY), Eligibility::Ready);
        assert_eq!(eligibility(&s, due + 3 + 3 * DAY), Eligibility::Unconfirmed);
        assert_eq!(
            s.candidate.as_ref().unwrap().eligibility(due + 2, Some("2.0.0")),
            Eligibility::Installed
        );
        s.normalize(due, false);
        assert_eq!(eligibility(&s, due), Eligibility::Waiting(due + DAY));
        s.normalize(1, false);
        assert_eq!(s.candidate.unwrap().first_seen, 1);
    }

    #[test]
    fn withdrawal_and_test_mode_start_new_acquisitions() {
        let mut s = initial();
        let old = s.candidate.as_ref().unwrap().attempt(10);
        for (index, replacement) in [
            None,
            Some(Identity {
                sha256: "different".into(),
                ..offer()
            }),
            Some(Identity { test: true, ..offer() }),
        ]
        .into_iter()
        .enumerate()
        {
            let ord = index as u64 * 3 + 3;
            checked(&mut s, ord, 11, 13, false, replacement);
            assert!(!checked(&mut s, ord - 1, 10, 14, false, Some(offer())));
            checked(&mut s, ord + 1, 15, 16, false, Some(offer()));
            assert_ne!(s.candidate.as_ref().unwrap().acquisition, old.acquisition);
            assert!(s.download_target(&old.acquisition, false).is_none());
            s.attempt = Some(old.clone());
            assert!(settle(&mut s, Settlement::Failed("old".into()), 17).is_none());
            assert_eq!(s.candidate.as_ref().unwrap().failures, 0);
        }
        let test = Identity { test: true, ..offer() };
        checked(&mut s, 20, 20, 20, true, Some(test.clone()));
        assert_eq!(eligibility(&s, 20), Eligibility::Unconfirmed);
        checked(&mut s, 21, 20, 21, true, Some(test));
        assert_eq!(eligibility(&s, 21), Eligibility::Ready);
        s.normalize(22, false);
        assert!(s.candidate.is_none());
    }

    #[test]
    fn staging_requires_the_same_eligible_acquisition_and_enabled_mode() {
        let test = Identity { test: true, ..offer() };
        let mut ready = State::default();
        checked(&mut ready, 1, 10, 10, true, Some(test.clone()));
        checked(&mut ready, 2, 11, 11, true, Some(test.clone()));
        let mut staged = ready.candidate.as_ref().unwrap().attempt(11);
        ready.attempt = Some(staged.clone());
        staged.txn = Some("staged".into());
        for case in [
            "ready",
            "marker removed",
            "withdrawn",
            "reacquired",
            "disabled",
            "stale",
        ] {
            let mut s = ready.clone();
            match case {
                "marker removed" => s.normalize(12, false),
                "withdrawn" => s.withdraw(),
                "reacquired" => {
                    s.withdraw();
                    checked(&mut s, 3, 12, 12, true, Some(test.clone()));
                    checked(&mut s, 4, 12, 12, true, Some(test.clone()));
                }
                _ => {}
            }
            let at = if case == "stale" { 4 * DAY } else { 12 };
            assert_eq!(
                s.authorize_staged(&staged, "1.0.0", case != "disabled", at),
                case == "ready"
            );
            assert_eq!(s.attempt.as_ref().unwrap().txn, staged.txn);
            settle(&mut s, Settlement::Interrupted, at);
            assert!(s.attempt.is_none());
            assert!(s.candidate.is_none_or(|c| c.failures == 0));
        }
    }

    #[test]
    fn persisted_uppercase_digest_preserves_acquisition_confirmation_and_attempt() {
        let mut s = initial();
        let at = DAY + 20;
        checked(&mut s, 2, at, at, false, Some(offer()));
        let c = s.candidate.as_mut().unwrap();
        c.identity.sha256.make_ascii_uppercase();
        s.attempt = Some(c.attempt(at));
        s.normalize(at, false);
        s.observe(3, 0, at, false, Some((offer(), vec![])), None);
        let c = s.candidate.as_ref().unwrap();
        assert_eq!(c.acquisition, "id-1");
        assert_eq!(c.eligibility(at, None), Eligibility::Ready);
        assert_eq!(s.attempt.as_ref().unwrap().identity, c.identity);
    }

    #[test]
    fn disabled_check_can_withdraw_but_cannot_acquire_or_confirm() {
        let mut s = initial();
        let now = DAY + 11;
        s.observe(2, now, now, false, Some((offer(), vec![])), None);
        assert_eq!(eligibility(&s, now), Eligibility::Unconfirmed);
        let replacement = Identity {
            sha256: "replacement".into(),
            ..offer()
        };
        s.observe(4, now, now, false, Some((replacement, vec![])), None);
        assert!(s.candidate.is_none());
        assert!(!checked(&mut s, 3, now, now, false, Some(offer())));
        checked(&mut s, 5, now, now, false, Some(offer()));
        assert_eq!(s.candidate.unwrap().acquisition, "id-5");
    }

    #[test]
    fn audit_recovery_needs_a_terminal_record_from_the_component() {
        let staged = Record::new(Source::Actual, "staged").txn("t");
        let claim = Record::new(Source::Claimed, "applied").txn("t");
        let other = Record::new(Source::Actual, "failed").txn("other");
        assert!(audit_terminal("t", vec![claim, other]).is_none());
        assert!(matches!(
            audit_terminal("t", vec![staged]),
            Some(Settlement::Interrupted)
        ));
        let actual = Record::new(Source::Actual, "rolled_back")
            .txn("t")
            .error("changed by another installation");
        assert!(
            matches!(audit_terminal("t",vec![actual]),Some(Settlement::RolledBack(reason)) if reason == "changed by another installation")
        );
    }

    #[test]
    fn component_unavailability_only_records_a_skip() {
        let mut s = initial();
        s.component_unavailable("id-1", "offline".into(), 2 * DAY);
        assert_eq!(s.candidate.as_ref().unwrap().failures, 0);
        assert!(s.attempt.is_none());
        assert_eq!(s.last.unwrap().reason, "offline");
    }

    #[test]
    fn only_conclusive_failures_count_once_and_three_stop_installation() {
        let mut s = initial();
        for (i, outcome) in [
            Settlement::Rejected("apply refused".into()),
            Settlement::Failed("other session".into()),
            Settlement::RolledBack("verification".into()),
        ]
        .into_iter()
        .enumerate()
        {
            s.attempt = Some(s.candidate.as_ref().unwrap().attempt(20));
            s.attempt.as_mut().unwrap().txn = Some("txn".into());
            for pending in [Settlement::Unavailable, Settlement::Active(Kind::Deb)] {
                assert!(settle(&mut s, pending, 20).is_none());
                assert!(s.attempt.is_some());
                assert_eq!(s.candidate.as_ref().unwrap().failures, i as u8);
            }
            assert!(settle(&mut s, outcome, 22).is_some());
            assert!(settle(&mut s, Settlement::Failed("duplicate".into()), 23).is_none());
            assert_eq!(s.candidate.as_ref().unwrap().failures, i as u8 + 1);
        }
        s.normalize(0, false);
        assert_eq!(eligibility(&s, 0), Eligibility::Failed);
        assert_eq!(s.last.as_ref().unwrap().reason, "verification");
        for (txn, outcome) in [
            (None, Settlement::Interrupted),
            (Some("expired staged"), Settlement::Interrupted),
            (Some("txn"), Settlement::Superseded),
            (Some("repair"), Settlement::NeedsRepair),
        ] {
            let mut attempt = s.candidate.as_ref().unwrap().attempt(30);
            attempt.txn = txn.map(str::to_owned);
            s.attempt = Some(attempt);
            assert!(settle(&mut s, outcome, 31).is_none());
            assert!(s.attempt.is_none());
            assert_eq!(s.candidate.as_ref().unwrap().failures, 3);
        }
        s.attempt = Some(s.candidate.as_ref().unwrap().attempt(40));
        settle(&mut s, Settlement::Applied, 41);
        assert!(s.attempt.is_none() && s.candidate.is_none());
    }
}
