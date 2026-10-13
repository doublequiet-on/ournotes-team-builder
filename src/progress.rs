//! Synchronous compute-thread notifications; the host forwards them to its UI.
use crate::{
    power::{Member, Snap},
    search::Hit,
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    ffi::{c_char, CString},
};
pub type Callback = extern "C" fn(*const c_char);
#[derive(Default)]
struct State {
    callback: Option<Callback>,
    members: Vec<String>,
    snaps: Vec<String>,
    song: Value,
    last: f64,
}
thread_local! {static STATE:RefCell<State>=RefCell::new(State::default());}
pub fn with_callback<T>(callback: Option<Callback>, run: impl FnOnce() -> T) -> T {
    struct Restore(State);
    impl Drop for Restore {
        fn drop(&mut self) {
            STATE.with(|s| {
                s.replace(std::mem::take(&mut self.0));
            });
        }
    }
    let previous = STATE.with(|s| {
        s.replace(State {
            callback,
            last: f64::NEG_INFINITY,
            ..State::default()
        })
    });
    let _restore = Restore(previous);
    run()
}
pub fn keys(members: &[Member], snaps: &[Snap]) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.callback.is_some() {
            s.members = members.iter().map(|m| m.key.clone()).collect();
            s.snaps = snaps.iter().map(|s| s.key.clone()).collect();
        }
    })
}
pub fn song(song: &Value) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.song = song.clone();
        s.last = f64::NEG_INFINITY;
    })
}
pub fn emit(hits: &[Hit]) {
    STATE.with(|state|{
    let mut state=state.borrow_mut();let Some(callback)=state.callback else{return};let now=crate::clock::milliseconds();if hits.is_empty() || now-state.last<250.0{return;}state.last=now;
    // Internal calibration may use a subset of the original box. Only the
    // outer API's indices are mapped here; those searches explicitly publish.
    if hits.iter().any(|h|h.team.members.iter().any(|&i|i>=state.members.len()) || h.team.snaps.iter().any(|&j|j>=state.snaps.len() as i32)){return;}
    let values:Vec<_>=hits.iter().map(|hit|json!({"song":state.song,"members":hit.team.members.iter().map(|&i|&state.members[i]).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(&state.snaps[j as usize])}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key})).collect();
    let message=CString::new(json!({"song":state.song,"hits":values}).to_string()).expect("JSON has no NUL");callback(message.as_ptr());
})
}
