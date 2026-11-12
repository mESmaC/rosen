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
        let on = |f: &&ChangeFacts| f.parent == ChangeParent::Change(*id);
        Ok(changes.values().filter(on).map(|f| f.id).collect())
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
        changes.get_mut(id).unwrap().parent = ChangeParent::Base(*onto);
        Ok(())
    }
    fn release_claims(&self, _: &ChangeId) -> LandResult<()> {
        Ok(())
    }
}

impl Merger for Shared {
    fn merge(&self, _: &CommitId, onto: &CommitId, head: &CommitId) -> LandResult<MergeOutcome> {
        Ok(MergeOutcome::Clean(oid(
            TREE,
            onto.as_bytes()[1] ^ head.as_bytes()[1],
        )))
    }
}

impl CommitWriter for Shared {
    fn write_landed(&self, commit: &LandedCommit) -> LandResult<CommitId> {
        let n = self.0.next.fetch_add(1, Ordering::SeqCst).to_be_bytes();
        let mut bytes = [LANDED; 20];
        bytes[12..].copy_from_slice(&n);
        let id = ObjectId::from_bytes(bytes);
        self.0.parents.lock().unwrap().insert(id, commit.base);
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
        world.changes.lock().unwrap().insert(facts.id, facts);
    }
    let deps = LandingDeps {
        book: Box::new(Shared(world.clone())),
        merger: Box::new(Shared(world.clone())),
        writer: Box::new(Shared(world.clone())),
        events: Box::new(Shared(world.clone())),
    };
    let ledger = Ledger::open_in_memory().unwrap();
    assert_eq!(
        ledger.init_base("main", &root()).unwrap(),
        InitBase::Created
    );
    // A project has one base (SPEC.md §2): a second import under another
    // branch name must not create a second base.
    let again = ledger.init_base("master", &oid(ROOT, 9)).unwrap();
    let existing = InitBase::Exists {
        name: "main".into(),
        commit: root(),
    };
    assert_eq!(again, existing);
    (
        world,
        Arc::new(Landing::new(ledger, deps, LandingConfig::default())),
    )
}

const ROOT: u8 = 1;
const HEAD: u8 = 2;
const TREE: u8 = 3;
const LANDED: u8 = 4;

/// A distinct object ID per (kind, number).
fn oid(kind: u8, n: u8) -> ObjectId {
    let mut bytes = [0; 20];
    bytes[0] = kind;
    bytes[1] = n;
    ObjectId::from_bytes(bytes)
}

fn root() -> CommitId {
    oid(ROOT, 0)
}

fn cid(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

fn change(n: u8, tier: Tier, parent: ChangeParent) -> ChangeFacts {
    ChangeFacts {
        id: cid(n),
        owner: AgentId::new(format!("agent-{n}")).unwrap(),
        description: format!("change {n}"),
        state: ChangeState::Ready,
        parent,
        head: oid(HEAD, n),
        merge_base: root(),
        tier,
        has_conflicts: false,
        approved_head: Some(oid(HEAD, n)),
        approvers: vec![AgentId::new("reviewer").unwrap()],
    }
}

#[test]
fn concurrent_lands_apply_once_on_a_linear_base() {
    let ids: Vec<u8> = (0..100).collect();
    let (world, landing) = setup(
        ids.iter()
            .map(|&n| change(n, Tier::ProseOnly, ChangeParent::Base(root())))
            .collect(),
    );
    // Every change is requested twice at once, as a retrying client would.
    let threads: Vec<_> = ids
        .iter()
        .chain(ids.iter())
        .map(|id| {
            let (landing, id) = (landing.clone(), cid(*id));
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
    while tip != root() {
        tip = parents[&tip];
        chain += 1;
    }
    assert_eq!(chain, 100);
}

#[test]
fn receipt_counts_only_across_prose_lands() {
    let (doc, a, b, on_a) = (1, 2, 3, 4);
    let (world, landing) = setup(vec![
        change(doc, Tier::ProseOnly, ChangeParent::Base(root())),
        change(a, Tier::Normal, ChangeParent::Base(root())),
        change(b, Tier::Normal, ChangeParent::Base(root())),
        change(on_a, Tier::Normal, ChangeParent::Change(cid(a))),
    ]);
    let receipt = |n: u8, base: CommitId| Receipt {
        change: cid(n),
        head: oid(HEAD, n),
        base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
    };
    for n in [a, b, on_a] {
        landing.submit_receipt(receipt(n, root())).unwrap();
    }
    let refused = LandOutcome::Refused(Refusal::ParentUnlanded);
    assert_eq!(landing.land(&cid(on_a)).unwrap(), refused);
    assert!(matches!(
        landing.land(&cid(doc)).unwrap(),
        LandOutcome::Landed { .. }
    ));
    // Only prose landed since a's receipt base, so the receipt still counts.
    assert!(matches!(
        landing.land(&cid(a)).unwrap(),
        LandOutcome::Landed { .. }
    ));
    let refused = LandOutcome::Refused(Refusal::NeedsRecheck);
    assert_eq!(landing.land(&cid(b)).unwrap(), refused);
    let base = landing.ledger().base("main").unwrap().unwrap();
    landing.submit_receipt(receipt(b, base)).unwrap();
    assert!(matches!(
        landing.land(&cid(b)).unwrap(),
        LandOutcome::Landed { .. }
    ));
    // The change stacked on `a` was moved onto the base `a` landed as.
    let events = world.events.lock().unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, LandingEvent::Rebased { change, .. } if *change == cid(on_a))));
}
