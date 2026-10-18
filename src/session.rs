//! Opaque, reusable contexts; cancellation never waits on a running search.
use crate::{api::Engine, clock, master};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};
struct Session {
    engine: Mutex<Engine>,
    generation: Arc<AtomicU64>,
}
static NEXT: AtomicU64 = AtomicU64::new(1);
static SESSIONS: OnceLock<Mutex<HashMap<u64, Arc<Session>>>> = OnceLock::new();
type MasterCache = Mutex<Option<(Value, master::Master)>>;
static MASTER: OnceLock<MasterCache> = OnceLock::new();
fn sessions() -> &'static Mutex<HashMap<u64, Arc<Session>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn get(handle: u64) -> Result<Arc<Session>, String> {
    sessions()
        .lock()
        .map_err(|_| "session-registry-poisoned")?
        .get(&handle)
        .cloned()
        .ok_or_else(|| "unknown-engine-context".into())
}
pub fn execute(input: &Value) -> Result<Value, String> {
    let op = input["op"].as_str().ok_or("missing-context-operation")?;
    if op == "create" {
        // Compare the complete release document, including cross-server data.
        // Worker contexts then share immutable tables rather than duplicating
        // the card/skill database for every CPU worker.
        let engine = {
            let mut cache = MASTER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .map_err(|_| "master-cache-poisoned")?;
            if cache
                .as_ref()
                .is_none_or(|(data, _)| data != &input["data"])
            {
                *cache = Some((input["data"].clone(), master::compile(&input["data"])?));
            }
            Engine::from_master(cache.as_ref().unwrap().1.clone())
        };
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        sessions()
            .lock()
            .map_err(|_| "session-registry-poisoned")?
            .insert(
                id,
                Arc::new(Session {
                    engine: Mutex::new(engine),
                    generation: Arc::new(AtomicU64::new(0)),
                }),
            );
        return Ok(json!({"handle":id}));
    }
    let handle = input["handle"].as_u64().ok_or("invalid-engine-context")?;
    if op == "dispose" {
        if let Some(session) = sessions()
            .lock()
            .map_err(|_| "session-registry-poisoned")?
            .remove(&handle)
        {
            session.generation.fetch_add(1, Ordering::Relaxed);
        }
        return Ok(json!({"disposed":true}));
    }
    let session = get(handle)?;
    if op == "cancel" {
        session.generation.fetch_add(1, Ordering::Relaxed);
        return Ok(json!({"cancelled":true}));
    }
    crate::with_cancellation(Arc::clone(&session.generation), || {
        let started = clock::milliseconds();
        let generation = crate::cancellation_generation();
        let mut engine = session
            .engine
            .lock()
            .map_err(|_| "engine-context-poisoned")?;
        if generation != crate::cancellation_generation() {
            return Err("cancelled".into());
        }
        match op {
            "qualify" => {
                Ok(json!({"issues":crate::pt::validate(&engine.master,&input["request"])?}))
            }
            "run" => engine.run(input, started, &clock::milliseconds),
            "growth" => engine.growth(input, started, &clock::milliseconds),
            "rates" => engine.rates(input),
            "evaluateAll" => engine.evaluate_all(input),
            "timeline" => engine.timeline(input),
            "chart" => {
                let song = &input["song"];
                let source = if input["source"].is_null() {
                    crate::runtime_chart::convert(&input["chart"])?
                } else {
                    serde_json::from_value(input["source"].clone()).map_err(|e| e.to_string())?
                };
                engine.insert_chart(
                    master::int(&song["songId"]),
                    master::int(&song["difficulty"]),
                    source,
                )?;
                Ok(json!({"ready":true}))
            }
            "evaluate" => engine.evaluate(
                &input["request"],
                &input["team"],
                master::int(&input["song"]["songId"]),
                master::int(&input["song"]["difficulty"]),
            ),
            _ => Err(format!("unknown-context-operation:{op}")),
        }
    })
}
