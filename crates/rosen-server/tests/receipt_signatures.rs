//! Risk: the base moves on a receipt its submitter's machine never signed
//! (SPEC.md §2 "Receipt", R8 "Accept when": the base only ever moves by a
//! land whose receipt matches). A receipt must verify when stored and again
//! when a land reads it, so a forged, altered or revoked machine's receipt
//! never counts.

use rosen_server::landing::*;
use rosen_server::machines::signing::MachineKey;

fn oid(kind: u8) -> ObjectId {
    ObjectId::from_bytes([kind; 20])
}

fn signed(key: &MachineKey, receipt: &Receipt, machine: &str) -> SignedReceipt {
    SignedReceipt {
        receipt: receipt.clone(),
        machine: machine.into(),
        signature: key.sign(&receipt.signing_payload()).signature,
    }
}

#[test]
fn only_a_trusted_machines_signature_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    let base = oid(1);
    ledger.init_base("main", &base).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO machines(id,name,role,status,last_contact,credential_hash,expires_at)
         VALUES ('m1','m1','checker','active',0,'h',9999999999)",
        [],
    )
    .unwrap();
    let (key, thief) = (MachineKey::generate(), MachineKey::generate());
    assert_eq!(
        ledger.bind_signing_key("m1", &key.public_key()).unwrap(),
        KeyBinding::Bound
    );
    // A bound key is never replaced, so a stolen credential cannot bring its own.
    assert_eq!(
        ledger.bind_signing_key("m1", &thief.public_key()).unwrap(),
        KeyBinding::Conflict
    );
    let receipt = Receipt {
        change: ChangeId::from_bytes([7; 16]),
        head: oid(2),
        base,
        check_set: "pre-land".into(),
        submitter: AgentId::new("checker").unwrap(),
    };
    assert!(ledger.put_receipt(&signed(&thief, &receipt, "m1")).is_err());
    ledger.put_receipt(&signed(&key, &receipt, "m1")).unwrap();
    let best = || {
        let (change, head) = (receipt.change, receipt.head);
        ledger.best_receipt("main", &change, &head, "pre-land", &base)
    };
    assert!(matches!(best().unwrap(), ReceiptLookup::Valid { .. }));
    // A row altered after its signature was checked no longer counts.
    db.execute("UPDATE receipts SET submitter = 'forged'", [])
        .unwrap();
    assert_eq!(best().unwrap(), ReceiptLookup::Invalid);
    db.execute("UPDATE receipts SET submitter = 'checker'", [])
        .unwrap();
    assert!(matches!(best().unwrap(), ReceiptLookup::Valid { .. }));
    // Nor does a receipt from a machine revoked since it was stored.
    db.execute("UPDATE machines SET status = 'revoked'", [])
        .unwrap();
    assert_eq!(best().unwrap(), ReceiptLookup::Invalid);
}
