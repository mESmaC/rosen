//! Risk: a quarantined member is handed another agent's message after all,
//! by releasing what it held while heads-down (SPEC R13: a quarantined
//! member gets nothing from other agents). The held delivery must survive
//! the quarantine and arrive only once the member is back in open mode.

use rosen_protocol::channel::{
    Author, ChannelAnswer, ChannelRequest, Intent, MessageMode, SendMessage,
};
use rosen_server::channel;
use rosen_server::events::EventLog;

fn set_mode(log: &EventLog, member: &str, mode: MessageMode) {
    let member = member.into();
    channel::run(log, ChannelRequest::SetMode { member, mode }).unwrap();
}

fn delivered_to(log: &EventLog, member: &str) -> Vec<String> {
    let call = ChannelRequest::Messages {
        reader: Some(member.into()),
        after: 0,
    };
    let ChannelAnswer::Messages(page) = channel::run(log, call).unwrap() else {
        panic!("messages answer");
    };
    page.messages.into_iter().map(|m| m.text).collect()
}

#[test]
fn quarantine_keeps_held_messages_held_until_open() {
    let log = EventLog::in_memory();
    for (member, nick) in [("a", "alice"), ("b", "bob")] {
        let join = ChannelRequest::Join {
            member: member.into(),
            nick: nick.into(),
            machine: "m".into(),
        };
        channel::run(&log, join).unwrap();
    }
    set_mode(&log, "b", MessageMode::HeadsDown);
    let send = SendMessage {
        from: Author::Agent { member: "a".into() },
        to: vec!["bob".into()],
        intent: Intent::Inform,
        text: "held".into(),
    };
    channel::run(&log, ChannelRequest::Send(send)).unwrap();
    set_mode(&log, "b", MessageMode::Quarantined);

    let release = || ChannelRequest::ReleaseHeld { member: "b".into() };
    let answer = channel::run(&log, release()).unwrap();
    assert_eq!(answer, ChannelAnswer::Delivered(0));
    assert!(delivered_to(&log, "b").is_empty());

    set_mode(&log, "b", MessageMode::Open);
    assert_eq!(delivered_to(&log, "b"), ["held"]);
}
