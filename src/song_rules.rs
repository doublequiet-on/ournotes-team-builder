//! Call-scoped song fields shared by Live, Gekiso, rewards and Arena.
use crate::master::Master;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn view(master: &Master, rule: &Value) -> Result<Master, String> {
    let music_type = rule["musicType"]
        .as_i64()
        .filter(|v| (0..=5).contains(v) || *v == 99)
        .ok_or("song-rule-type")? as i32;
    let values = rule["missions"]
        .as_array()
        .filter(|v| v.len() == 3)
        .ok_or("song-rule-missions")?;
    let missions = values
        .iter()
        .map(|v| {
            v.as_i64()
                .filter(|n| (0..=3).contains(n))
                .map(|n| n as i32)
                .ok_or("song-rule-missions")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut scoped = master.clone();
    for song in Arc::make_mut(&mut scoped.songs).values_mut() {
        if music_type != 0 {
            song.music_type = music_type;
        }
        for (index, &mission) in missions.iter().enumerate() {
            if mission != 0 {
                song.missions.resize(song.missions.len().max(3), 0);
                song.missions[index] = mission;
            }
        }
    }
    if music_type != 0 {
        for song in Arc::make_mut(&mut scoped.challenge_music) {
            song.music_type = music_type;
        }
    }
    Ok(scoped)
}
