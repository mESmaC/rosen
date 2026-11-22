import AutoRevert.Design

private def bit (b : Bool) : String := if b then "1" else "0"

private def emptyPlatformState : AutoRevert.PlatformState :=
  { revert := {
      base := 1
      reverted := []
      openChange := fun _ => false
      snapshots := fun _ => [1]
      owner := fun _ => 1
      output := fun _ => ""
    }
    inverses := []
    records := [] }

private def recordRevert (records : List AutoRevert.FailureRecord) (key : AutoRevert.FailureKey) :
    Option Nat :=
  (records.find? (·.key == key)).bind (·.revert)

def main : IO Unit := do
  IO.println "already\tmoved\tclean\trecovered\tland"
  for already in [false, true] do
    for moved in [false, true] do
      for clean in [false, true] do
        for recovered in [false, true] do
          IO.println s!"{bit already}\t{bit moved}\t{bit clean}\t{bit recovered}\t{bit (AutoRevert.readyGate already moved clean recovered)}"
  -- R12 mode & advisory platform revert disposition.
  for beforeLand in [true, false] do
    for isBuild in [false, true] do
      for isAdvisory in [false, true] do
        let review := if beforeLand then "before-land" else "after-land"
        let kind := if isBuild then "build" else "test"
        let rev := AutoRevert.requiresRevert beforeLand isBuild isAdvisory
        IO.println s!"mode\t{review}\t{kind}\t{bit isAdvisory}\t{bit rev}"
  -- R12 multi-platform failure records sharing one culprit-keyed inverse.
  let scenarios : List (Bool × Bool × String × Bool × Bool × String × Bool) := [
    (false, true, "windows", false, true, "macos", false),
    (false, true, "windows", false, true, "windows", false),
    (false, true, "windows", false, true, "linux", true),
    (false, true, "linux", true, true, "windows", false),
    (false, false, "windows", false, false, "macos", false),
    (true, false, "windows", false, false, "macos", false),
    (false, false, "windows", false, true, "macos", false)
  ]
  for (beforeLand, b1, p1, a1, b2, p2, a2) in scenarios do
    let review := if beforeLand then "before-land" else "after-land"
    let k1 := if b1 then "build" else "test"
    let k2 := if b2 then "build" else "test"
    let (ps1, _) := AutoRevert.recordPlatformFailure emptyPlatformState beforeLand b1 a1 p1 1 1 1 2 true true "fail1"
    let (ps2, _) := AutoRevert.recordPlatformFailure ps1 beforeLand b2 a2 p2 ps1.revert.base 1 1 3 true true "fail2"
    let r1 := recordRevert ps2.records { culprit := 1, isBuild := b1, platform := p1 }
    let r2 := recordRevert ps2.records { culprit := 1, isBuild := b2, platform := p2 }
    let shared := r1.isSome && r1 == r2
    IO.println s!"platform\t{review}\t{k1}\t{p1}\t{bit a1}\t{k2}\t{p2}\t{bit a2}\t{ps2.records.length}\t{ps2.inverses.length}\t{bit shared}\t{bit r1.isSome}\t{bit r2.isSome}"

#print axioms main
