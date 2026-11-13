/-! Claim release notifications (SPEC.md R6). Models `claims::store::release_in`
and `claims::delivery`: one transaction deletes the released records and adds one
`claim_release_delivery` note of owed events; recovery appends a note's events,
then deletes it. Which waits wake is `Claims.correct`; here they are inputs. -/
namespace ClaimRelease

/-- Durable rows: a claim record, or a pending release note. -/
inductive Key where
  | record (r : Nat)
  | note (n : Nat)
  deriving DecidableEq

/-- The metadata store: one optional row per key. A note holds event IDs. -/
abbrev Store := Key → Option (List Nat)

/-- One row write: an INSERT, a single-row UPDATE or a DELETE (`none`). -/
def put (s : Store) (w : Key × Option (List Nat)) : Store :=
  fun k => if k = w.1 then w.2 else s k

def runWrites (s : Store) (ws : List (Key × Option (List Nat))) : Store := ws.foldl put s

private theorem changed_written :
    ∀ (ws : List (Key × Option (List Nat))) (s : Store) (k : Key),
      runWrites s ws k ≠ s k → k ∈ ws.map Prod.fst
  | [], _, _, h => absurd rfl h
  | w :: ws, s, k, h => by
    by_cases hk : k = w.1
    · simp [hk]
    · have := changed_written ws (put s w) k fun he =>
        h (show runWrites (put s w) ws k = s k by rw [he]; simp [put, hk])
      simp [this]

/-- Every row that must change needs a write of its own. -/
private theorem writes_lower (s : Store) (ws : List (Key × Option (List Nat)))
    (ks : List Key) (hn : ks.Nodup) (hd : ∀ k ∈ ks, runWrites s ws k ≠ s k) :
    ks.length ≤ ws.length := by
  have hsub : ks ⊆ ws.map Prod.fst := fun k hk => changed_written ws s k (hd k hk)
  simpa using hn.length_le_of_subset hsub

private theorem run_untouched (s : Store) (ws : List (Key × Option (List Nat))) (k : Key)
    (h : k ∉ ws.map Prod.fst) : runWrites s ws k = s k :=
  Classical.byContradiction fun hne => h (changed_written ws s k hne)

/-- The release transaction's writes: delete every released record, then add
one note for the owed events when there are any (`delivery::enqueue`). -/
def releaseWrites (next : Nat) (removed events : List Nat) : List (Key × Option (List Nat)) :=
  removed.map (fun r => (Key.record r, none)) ++
    (if events = [] then [] else [(Key.note next, some events)])

/-- `owed` is ghost state: every event a committed release promised. -/
structure State where
  store : Store
  next : Nat
  log : List Nat
  owed : List Nat

/-- A failed transaction (`commit = false`) rolls back every write. -/
def release (s : State) (removed events : List Nat) (commit : Bool) : State :=
  if commit then
    { store := runWrites s.store (releaseWrites s.next removed events), next := s.next + 1,
      log := s.log, owed := s.owed ++ events }
  else s

/-- Append the first `k` events of note `n`; delete the note only after all
of them are appended. `k` short of the note's length is a crash. -/
def publish (s : State) (n k : Nat) : State :=
  match s.store (.note n) with
  | none => s
  | some es =>
    { s with log := s.log ++ es.take k,
             store := if es.length ≤ k then put s.store (.note n, none) else s.store }

inductive Step where
  | release (removed events : List Nat) (commit : Bool)
  | publish (n k : Nat)
def step (s : State) : Step → State
  | .release r e c => release s r e c
  | .publish n k => publish s n k

def run (s : State) (trace : List Step) : State := trace.foldl step s
def start (st : Store) : State := ⟨st, 0, [], []⟩

/-- Restart recovery: deliver every note completely, oldest first. -/
def replay (s : State) : State :=
  (List.range s.next).foldl (fun t n => publish t n ((t.store (.note n)).getD []).length) s

private def Inv (s : State) : Prop :=
  (∀ e ∈ s.owed, e ∈ s.log ∨ ∃ n es, s.store (.note n) = some es ∧ e ∈ es) ∧
  (∀ e ∈ s.log, e ∈ s.owed) ∧
  (∀ n es, s.store (.note n) = some es → n < s.next ∧ ∀ e ∈ es, e ∈ s.owed)

private theorem release_note (s : State) (removed events : List Nat) (m : Nat) :
    runWrites s.store (releaseWrites s.next removed events) (.note m) =
      if m = s.next ∧ events ≠ [] then some events else s.store (.note m) := by
  have hu : runWrites s.store (removed.map fun r => (Key.record r, none)) (.note m) =
      s.store (.note m) := run_untouched _ _ _ (by simp)
  by_cases he : events = []
  · simp [releaseWrites, he, runWrites] at hu ⊢; exact hu
  · simp only [runWrites] at hu
    by_cases hm : m = s.next <;> simp [releaseWrites, runWrites, put, hm, he, hu]

private theorem deletes_record : ∀ (removed : List Nat) (s : Store) (r : Nat), r ∈ removed →
    runWrites s (removed.map fun r => (Key.record r, none)) (.record r) = none
  | [], _, _, h => absurd h (by simp)
  | a :: rest, s, r, h => by
    show runWrites (put s (.record a, none)) (rest.map fun r => (Key.record r, none)) _ = none
    by_cases hr : r ∈ rest
    · exact deletes_record rest _ r hr
    · rw [run_untouched _ _ _ (by simpa using hr)]; simp [put, (by simpa [hr] using h : r = a)]

private theorem release_record (s : State) (removed events : List Nat) (r : Nat)
    (h : r ∈ removed) : (release s removed events true).store (.record r) = none := by
  have hd := deletes_record removed s.store r h
  by_cases he : events = []
  · simp [release, releaseWrites, he, runWrites] at hd ⊢; exact hd
  · simp only [runWrites] at hd; simp [release, releaseWrites, he, runWrites, put, hd]

private theorem inv_release (s : State) (removed events : List Nat) (c : Bool) (h : Inv s) :
    Inv (release s removed events c) := by
  cases c
  · simpa [release] using h
  change Inv ⟨runWrites s.store (releaseWrites s.next removed events), s.next + 1, s.log,
    s.owed ++ events⟩
  obtain ⟨ho, hl, hn⟩ := h
  refine ⟨?_, ?_, ?_⟩
  · intro e he
    rcases List.mem_append.mp he with he | he
    · refine (ho e he).imp id fun ⟨n, es, hs, hm⟩ => ⟨n, es, ?_, hm⟩
      dsimp only; rw [release_note]; simp [Nat.ne_of_lt (hn n es hs).1, hs]
    · exact Or.inr ⟨s.next, events, by
        dsimp only; rw [release_note]; simp [List.ne_nil_of_mem he], he⟩
  · intro e he; exact List.mem_append_left _ (hl e he)
  · intro m es hs
    dsimp only at hs ⊢
    rw [release_note] at hs
    split at hs
    · rename_i hm; cases hs; exact ⟨by omega, fun e he => List.mem_append_right _ he⟩
    · have := hn m es hs; exact ⟨by omega, fun e he => List.mem_append_left _ (this.2 e he)⟩

private theorem publish_keeps (s : State) (n k : Nat) :
    (publish s n k).next = s.next ∧ (publish s n k).owed = s.owed := by
  unfold publish; split <;> simp

private theorem publish_note (s : State) (n k m : Nat) (hm : s.store (.note m) = none) :
    (publish s n k).store (.note m) = none := by
  unfold publish; split
  · exact hm
  · simp only; split <;> simp [put, hm]

private theorem inv_publish (s : State) (n k : Nat) (h : Inv s) : Inv (publish s n k) := by
  obtain ⟨ho, hl, hn⟩ := h
  unfold publish
  split
  · exact ⟨ho, hl, hn⟩
  rename_i es hes
  have hsub := (hn n es hes).2
  refine ⟨?_, ?_, ?_⟩
  · intro e he
    rcases ho e he with hlog | ⟨m, es', hs, hm⟩
    · exact Or.inl (List.mem_append_left _ hlog)
    · by_cases hack : es.length ≤ k ∧ m = n
      · obtain ⟨hk, rfl⟩ := hack
        rw [hes] at hs; cases hs
        exact Or.inl (List.mem_append_right _ (by rwa [List.take_of_length_le hk]))
      · refine Or.inr ⟨m, es', ?_, hm⟩
        simp only; split
        · have : m ≠ n := fun hmn => hack ⟨by assumption, hmn⟩
          simp [put, this, hs]
        · exact hs
  · intro e he
    exact (List.mem_append.mp he).elim (hl e) fun he => hsub e (List.mem_of_mem_take he)
  · intro m es' hs
    simp only at hs ⊢
    split at hs
    · by_cases hmn : m = n <;> simp [put, hmn] at hs; exact hn m es' hs
    · exact hn m es' hs

private theorem inv_run : ∀ (trace : List Step) (s : State), Inv s → Inv (run s trace)
  | [], _, h => h
  | .release r e c :: rest, s, h => inv_run rest _ (inv_release s r e c h)
  | .publish n k :: rest, s, h => inv_run rest _ (inv_publish s n k h)

private theorem replay_fold : ∀ (L : List Nat) (s : State), Inv s →
    let t := L.foldl (fun t n => publish t n ((t.store (.note n)).getD []).length) s
    Inv t ∧ t.next = s.next ∧ t.owed = s.owed ∧
      ∀ m, (m ∈ L ∨ s.store (.note m) = none) → t.store (.note m) = none
  | [], s, h => ⟨h, rfl, rfl, fun m hm => by simpa using hm⟩
  | n :: L, s, h => by
    let s' := publish s n ((s.store (.note n)).getD []).length
    have hcl : s'.store (.note n) = none := by
      show (publish s n _).store _ = none
      unfold publish; split <;> rename_i hes <;> simp_all [put]
    obtain ⟨hi, hx, ho, hz⟩ := replay_fold L s' (inv_publish s n _ h)
    have ⟨hpn, hpo⟩ := publish_keeps s n ((s.store (.note n)).getD []).length
    refine ⟨hi, hx.trans hpn, ho.trans hpo, fun m hm => hz m ?_⟩
    rcases hm with hm | hm
    · exact (List.mem_cons.mp hm).elim (fun h => Or.inr (h ▸ hcl)) Or.inl
    · exact Or.inr (publish_note s n _ m hm)

/-- **correct** (SPEC.md §R6, "Release transactions retain pending
notifications until published ... Delivery is at least once"): from a store
with no pending notes, after any trace of committed or failed releases,
partial (crashed) and complete deliveries, every owed event is in the log or
in a pending note, the log holds only owed events, a failed release changes
nothing, a committed one removes every released record, and restart recovery
publishes exactly the owed events. -/
-- SPEC.md §R6
theorem correct (st : Store) (empty : ∀ n, st (.note n) = none) (trace : List Step) :
    let s := run (start st) trace
    (∀ e ∈ s.owed, e ∈ s.log ∨ ∃ n es, s.store (.note n) = some es ∧ e ∈ es) ∧
    (∀ e ∈ s.log, e ∈ s.owed) ∧
    (∀ removed events, release s removed events false = s) ∧
    (∀ removed events, ∀ r ∈ removed, (release s removed events true).store (.record r) = none) ∧
    (∀ e, e ∈ (replay s).log ↔ e ∈ s.owed) := by
  intro s
  have h0 : Inv (start st) :=
    ⟨by simp [start], by simp [start], fun n es hs => by simp [start, empty] at hs⟩
  have hs : Inv s := inv_run trace _ h0
  obtain ⟨ho, hl, hn⟩ := hs
  refine ⟨ho, hl, fun _ _ => by simp [release], fun r e => release_record s r e, ?_⟩
  obtain ⟨⟨ro, rl, rn⟩, rx, row, rz⟩ := replay_fold (List.range s.next) s ⟨ho, hl, hn⟩
  refine fun e => ⟨fun he => row ▸ rl e he, fun he => ?_⟩
  rcases ro e (row ▸ he) with hlog | ⟨m, es, hm, _⟩
  · exact hlog
  · rw [rz m (Or.inl (List.mem_range.mpr (rx ▸ (rn m es hm).1)))] at hm; cases hm

/-- **cost_bound** (SPEC.md §R6). Cost model: durable row writes in the
release transaction (INSERT, single-row UPDATE or DELETE). Reads, the event
appends and acknowledgement of delivery, and crash retries are excluded. A
release writes one delete per released record and at most one note. -/
-- SPEC.md §R6
theorem cost_bound (next : Nat) (removed events : List Nat) :
    (releaseWrites next removed events).length =
        removed.length + (if events = [] then 0 else 1) ∧
      (releaseWrites next removed events).length ≤ removed.length + 1 := by
  by_cases he : events = [] <;> simp [releaseWrites, he]

/-- An alternative release design: the writes its release transaction makes,
and the events a restart publishes from the stored rows. -/
structure Protocol where
  commit : Store → List Nat → List Nat → List (Key × Option (List Nat))
  recover : Store → List Nat

/-- The class of alternatives: crash-safe designs. The commit deletes every
released record; a restart right after the commit publishes every event the
release owes; and recovery reads pending notifications, not claim records
(a deleted record leaves nothing to read). -/
def CrashSafe (p : Protocol) : Prop :=
  (∀ s t : Store, (∀ n, s (.note n) = t (.note n)) → p.recover s = p.recover t) ∧
  ∀ s removed events,
    (∀ r ∈ removed, runWrites s (p.commit s removed events) (.record r) = none) ∧
    (∀ e ∈ events, e ∈ p.recover (runWrites s (p.commit s removed events)))

/-- **optimal** (SPEC.md §R6): releasing distinct stored records that owe
events not already pending, no crash-safe design writes fewer rows in its
release transaction than ours, under the cost model of `cost_bound`. -/
-- SPEC.md §R6
theorem optimal (p : Protocol) (hp : CrashSafe p) (s : Store) (next : Nat)
    (removed events : List Nat) (distinct : removed.Nodup)
    (stored : ∀ r ∈ removed, s (.record r) ≠ none)
    (fresh : ∀ e ∈ events, e ∉ p.recover s) :
    (releaseWrites next removed events).length ≤ (p.commit s removed events).length := by
  obtain ⟨hnotes, hsafe⟩ := hp
  obtain ⟨hdel, hpub⟩ := hsafe s removed events
  let ws := p.commit s removed events
  have hrec : ∀ k ∈ removed.map Key.record, runWrites s ws k ≠ s k := fun k hk => by
    obtain ⟨r, hr, rfl⟩ := List.mem_map.mp hk; rw [hdel r hr]; exact (stored r hr).symm
  have hnd : (removed.map Key.record).Nodup :=
    List.Pairwise.map Key.record (fun _ _ hab heq => hab (Key.record.inj heq)) distinct
  rw [(cost_bound next removed events).1]
  by_cases he : events = []
  · have h := writes_lower s ws _ hnd hrec
    rw [List.length_map] at h; split <;> first | omega | contradiction
  · obtain ⟨e, he'⟩ := List.exists_mem_of_ne_nil events he
    have hdiff : ∃ n, runWrites s ws (.note n) ≠ s (.note n) :=
      Classical.byContradiction fun hall => by
        have heq := hnotes _ s fun n => Classical.byContradiction fun hne => hall ⟨n, hne⟩
        exact fresh e he' (heq ▸ hpub e he')
    obtain ⟨n, hn⟩ := hdiff
    have := writes_lower s ws (.note n :: removed.map Key.record)
      (List.nodup_cons.mpr ⟨by simp, hnd⟩)
      (fun k hk => (List.mem_cons.mp hk).elim (fun h => h ▸ hn) (hrec k))
    simp only [List.length_cons, List.length_map] at this; split <;> first | omega | contradiction

#print axioms releaseWrites
#print axioms release
#print axioms publish
#print axioms run
#print axioms replay
#print axioms CrashSafe
#print axioms correct
#print axioms cost_bound
#print axioms optimal
end ClaimRelease
