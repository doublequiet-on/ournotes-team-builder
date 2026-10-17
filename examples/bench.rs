use haneoka_team_engine::session;
use serde_json::{json, Value};
use std::time::Instant;
fn main() {
    let path = std::env::args().nth(1).expect("input");
    let input: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let t = Instant::now();
    let created = session::execute(&json!({"op":"create","data":input["data"]})).unwrap();
    eprintln!("create {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let handle = created["handle"].clone();
    for round in 0..2 {
        let t = Instant::now();
        let out = session::execute(&json!({"op":"run","handle":handle,"request":input["request"],"charts":input["charts"]})).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let results = out["results"].as_array().cloned().unwrap_or_default();
        for r in results.iter().take(5) {
            println!("{} {} {} {}", r["song"]["songId"], r["proven"], r["hits"][0]["key"], r["stats"]);
        }
        println!("RUST native round {} {} songs {:.0} ms proven {}", round, results.len(), ms, results.iter().filter(|r| r["proven"] == true).count());
    }
}
