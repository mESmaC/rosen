//! Risk: concurrent lands applying a change twice, forking the base, or
//! reusing a receipt across a non-prose land (SPEC R8 "Accept when").

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use rosen_server::landing::*;

#[derive(Default)]
struct World {
    changes: Mutex<HashMap<ChangeId, ChangeFacts>>,
    parents: Mutex<HashMap<CommitId, CommitId>>,
    events: Mutex<Vec<LandingEvent>>,
    next: AtomicU64,
}

struct Shared(Arc<World>);

impl ChangeBook for Shared {
    fn facts(&self, id: &ChangeId) -> LandResult<Option<ChangeFacts>> {
        Ok(self.0.changes.lock().unwrap().get(id).cloned())
    }
    fn stacked_on(&self, id: &ChangeId) -> LandResult<Vec<ChangeId>> {
        let changes = self.0.changes.lock().unwrap();
        let on = |f: &&ChangeFacts| f.parent == Parent::Change(id.clone());
        Ok(changes.values().filter(on).map(|f| f.id.clone()).collect())
    }
    fn overlapping(&self, _: &ChangeId) -> LandResult<Vec<ChangeId>> {
        Ok(Vec::new())
    }
    fn mark_landed(&self, id: &ChangeId, _: &CommitId) -> LandResult<()> {
        self.0.changes.lock().unwrap().get_mut(id).unwrap().state = ChangeState::Landed;
        Ok(())
    }
    fn record_conflicts(&self, _: &ChangeId, _: &[String]) -> LandResult<()> {
        Ok(())
    }
    fn reparent(&self, id: &ChangeId, onto: &CommitId, _: Option<&TreeId>) -> LandResult<()> {
        let mut changes = self.0.changes.lock().unwrap();
        changes.get_mut(id).unwrap().parent = Parent::Base(onto.clone());
        Ok(())
    }
    fn release_claims(&self, _: &ChangeId) -> LandResult<()> {
        Ok(())
    }
}

impl Merger for Shared {
    fn merge(&self, _: &CommitId, onto: &CommitId, head: &CommitId) -> LandResult<MergeOutcome> {
        Ok(MergeOutcome::Clean(TreeId(format!("{onto}+{head}"))))
    }
}

impl CommitWriter for Shared {
    fn write_landed(&self, commit: &LandedCommit) -> LandResult<CommitId> {
        let id = CommitId(format!("c{}", self.0.next.fetch_add(1, Ordering::SeqCst)));
        self.0
            .parents
            .lock()
            .unwrap()
            .insert(id.clone(), commit.parent.clone());
        Ok(id)
    }
}

impl EventSink for Shared {
    fn emit(&self, event: LandingEvent) {
        self.0.events.lock().unwrap().push(event);
    }
}

fn setup(changes: Vec<ChangeFacts>) -> (Arc<World>, Arc<Landing>) {
    let world = Arc::new(World::default());
    for facts in changes {
        world
            .changes
            .lock()
            .unwrap()
            .insert(facts.id.clone(), facts);
    }
    let deps = LandingDeps {
        book: Box::new(Shared(world.clone())),
        merger: Box::new(Shared(world.clone())),
        writer: Box::new(Shared(world.clone())),
        events: Box::new(Shared(world.clone())),
    };
    let ledger = Ledger::open_in_memory().unwrap();
    ledger.init_base("main", &"c-root".into()).unwrap();
    (
        world,
        Arc::new(Landing::new(ledger, deps, LandingConfig::default())),
    )
}

fn change(id: &str, tier: Tier, parent: Parent) -> ChangeFacts {
    ChangeFacts {
        id: id.into(),
        owner: format!("agent-{id}"),
        state: ChangeState::Ready,
        parent,
        head: format!("h-{id}").as_str().into(),
        merge_base: "c-root".into(),
        tier,
        has_conflicts: false,
        approved_head: Some(format!("h-{id}").as_str().into()),
    }
}

#[test]
fn concurrent_lands_apply_once_on_a_linear_base() {
    let root = Parent::Base("c-root".into());
    let ids: Vec<String> = (0..100).map(|n| format!("p{n}")).collect();
    let (world, landing) = setup(
        ids.iter()
            .map(|id| change(id, Tier::ProseOnly, root.clone()))
            .collect(),
    );
    // Every change is requested twice at once, as a retrying client would.
    let threads: Vec<_> = ids
        .iter()
        .chain(ids.iter())
        .map(|id| {
            let (landing, id) = (landing.clone(), ChangeId(id.clone()));
            thread::spawn(move || landing.land(&id).unwrap())
        })
        .collect();
    let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let landed = outcomes
        .iter()
        .filter(|o| matches!(o, LandOutcome::Landed { .. }))
        .count();
    assert_eq!(landed, 100);
    // Walking parents from the tip must visit every land exactly once.
    let parents = world.parents.lock().unwrap();
    let mut tip = landing.ledger().base("main").unwrap().unwrap();
    let mut chain = 0;
    while tip.0 != "c-root" {
        tip = parents[&tip].clone();
        chain += 1;
    }
    assert_eq!(chain, 100);
}

#[test]
fn receipt_counts_only_across_prose_lands() {
    let root = Parent::Base("c-root".into());
    let (world, landing) = setup(vec![
        change("doc", Tier::ProseOnly, root.clone()),
        change("a", Tier::Normal, root.clone()),
        change("b", Tier::Normal, root),
        change("on-a", Tier::Normal, Parent::Change("a".into())),
    ]);
    let receipt = |id: &str, base: CommitId| Receipt {
        change: id.into(),
        head: format!("h-{id}").as_str().into(),
        base,
        check_set: "pre-land".into(),
        submitter: "checker".into(),
    };
    for id in ["a", "b", "on-a"] {
        landing
            .submit_receipt(receipt(id, "c-root".into()))
            .unwrap();
    }
    let refused = LandOutcome::Refused(Refusal::ParentUnlanded);
    assert_eq!(landing.land(&"on-a".into()).unwrap(), refused);
    assert!(matches!(
        landing.land(&"doc".into()).unwrap(),
        LandOutcome::Landed { .. }
    ));
    // Only prose landed since a's receipt base, so the receipt still counts.
    assert!(matches!(
        landing.land(&"a".into()).unwrap(),
        LandOutcome::Landed { .. }
    ));
    let refused = LandOutcome::Refused(Refusal::NeedsRecheck);
    assert_eq!(landing.land(&"b".into()).unwrap(), refused);
    let base = landing.ledger().base("main").unwrap().unwrap();
    landing.submit_receipt(receipt("b", base.clone())).unwrap();
    assert!(matches!(
        landing.land(&"b".into()).unwrap(),
        LandOutcome::Landed { .. }
    ));
    // The change stacked on `a` was moved onto the base `a` landed as.
    let events = world.events.lock().unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, LandingEvent::Rebased { change, .. } if change.0 == "on-a")));
}
