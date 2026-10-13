//! Engine context and platform-neutral team evaluation. Chart loading is owned
//! by the caller; the same release-bound context can serve native and WASM.
use crate::{
    chart::{self, Chart, Source},
    live,
    master::{self, Master},
    power::{self, MemberInput, SnapInput},
    skills,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
pub struct Engine {
    pub master: Master,
    pub charts: BTreeMap<(i32, i32), Chart>,
}
impl Engine {
    pub fn compile(data: &Value) -> Result<Self, String> {
        Ok(Self {
            master: master::compile(data)?,
            charts: BTreeMap::new(),
        })
    }
    pub fn insert_chart(
        &mut self,
        song_id: i32,
        difficulty: i32,
        source: Source,
    ) -> Result<(), String> {
        let song = self
            .master
            .songs
            .get(&song_id)
            .ok_or_else(|| format!("unknown-song:{song_id}"))?;
        let level = song
            .difficulties
            .iter()
            .find(|r| r.difficulty == difficulty)
            .ok_or_else(|| format!("unknown-chart:{song_id}:{difficulty}"))?;
        if self.charts.len() >= 4 && !self.charts.contains_key(&(song_id, difficulty)) {
            if let Some(key) = self.charts.keys().next().copied() {
                self.charts.remove(&key);
            }
        }
        self.charts.insert(
            (song_id, difficulty),
            chart::compile(&self.master, song, level, &source),
        );
        Ok(())
    }
    pub fn evaluate(
        &self,
        request: &Value,
        team: &Value,
        song_id: i32,
        difficulty: i32,
    ) -> Result<Value, String> {
        let inputs: Vec<MemberInput> =
            serde_json::from_value(request["members"].clone()).map_err(|e| e.to_string())?;
        let snaps: Vec<SnapInput> =
            serde_json::from_value(request["snaps"].clone()).map_err(|e| e.to_string())?;
        let cards = power::resolve_box(
            &self.master,
            &inputs,
            &snaps,
            request["unknownPolicy"] != "min",
        );
        let keys = team["members"].as_array().ok_or("missing team members")?;
        if keys.len() != 5 {
            return Err("a team requires five members".into());
        }
        let members = keys
            .iter()
            .map(|key| {
                cards
                    .members
                    .iter()
                    .find(|m| key.as_str() == Some(m.key.as_str()))
                    .cloned()
                    .ok_or_else(|| format!("unknown member:{key}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let snap_keys = team["snaps"].as_array().ok_or("missing team snaps")?;
        if snap_keys.len() != 5 {
            return Err("a team requires five snap slots".into());
        }
        let snaps = snap_keys
            .iter()
            .map(|key| {
                if key.is_null() {
                    Ok(None)
                } else {
                    cards
                        .snaps
                        .iter()
                        .find(|s| key.as_str() == Some(s.key.as_str()))
                        .cloned()
                        .map(Some)
                        .ok_or_else(|| format!("unknown snap:{key}"))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let player = power::player_state(&self.master, &request["player"]);
        let challenge_event = request["challengeEventId"]
            .as_i64()
            .or_else(|| request["goal"]["challengeEventId"].as_i64())
            .map(|id| id as i32);
        let music = power::music_view(&self.master, song_id, challenge_event)?;
        let effects = challenge_event
            .and_then(|id| self.master.events.get(&id))
            .map(|e| e.effects.as_slice())
            .unwrap_or_default();
        let power = power::team_power(
            &self.master,
            &player,
            &members,
            &snaps,
            0,
            Some(&music),
            effects,
        )?;
        let mut unsupported = Vec::new();
        let slots = members
            .iter()
            .zip(&snaps)
            .map(|(member, snap)| {
                skills::resolve(&self.master, member, snap.as_ref(), &mut unsupported)
            })
            .collect();
        let chart = self
            .charts
            .get(&(song_id, difficulty))
            .ok_or_else(|| format!("missing-chart:{song_id}:{difficulty}"))?;
        let play = if request["play"].is_null() {
            &request["goal"]["play"]
        } else {
            &request["play"]
        };
        let judgements = live::judgements(chart.times.len(), play);
        let scores = live::score(&chart::prepare(
            &self.master,
            chart,
            power.total as i32,
            slots,
            judgements,
            1.0,
            100.0,
        ))?;
        let mut ranks: BTreeMap<i32, i32> = BTreeMap::new();
        for &value in &scores.scores {
            *ranks.entry(chart::score_rank(chart, value)).or_default() += 1;
        }
        let keys_of = |order: usize| live::orders()[order].map(|slot| members[slot].key.clone());
        let mut sorted = scores.scores.clone();
        sorted.sort_by(f64::total_cmp);
        Ok(
            json!({"song":{"songId":song_id,"difficulty":difficulty},"power":power.total,"slotPowers":power.slots,"leaderPercents":power.leader_percents,"unknownCards":cards.unknown,"unsupported":unsupported,"score":{"mean":scores.mean,"min":scores.min,"max":scores.max,"median":(sorted[59]+sorted[60])/2.0,"worstOrder":keys_of(scores.min_order),"bestOrder":keys_of(scores.max_order),"orders":scores.scores.iter().map(|v|crate::num::round(*v)).collect::<Vec<_>>(),"ranks":ranks}}),
        )
    }
}

struct PowerObjective;
impl crate::search::Objective for PowerObjective {
    fn skill(&mut self, _member: usize, _snap: i32) -> (f64, f64) {
        (0.0, 0.0)
    }
    fn bound(&mut self, totals: crate::search::Totals) -> f64 {
        totals.power
    }
    fn interval(
        &mut self,
        _team: &crate::search::Team,
        totals: crate::search::Totals,
    ) -> (f64, f64) {
        (totals.power, totals.power)
    }
    fn exact(
        &mut self,
        _team: &crate::search::Team,
        power: f64,
        _slots: &[f64],
    ) -> Result<crate::search::Exact, String> {
        Ok(crate::search::Exact {
            key: power,
            detail: Value::Null,
        })
    }
}
fn constraints(
    input: &Value,
    members: &[power::Member],
    snaps: &[power::Snap],
) -> crate::search::Constraints {
    let member_index: BTreeMap<_, _> = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.key.as_str(), i))
        .collect();
    let snap_index: BTreeMap<_, _> = snaps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.key.as_str(), i))
        .collect();
    let member_ids = |v: &Value| {
        master::rows(v)
            .iter()
            .filter_map(|v| v.as_str().and_then(|key| member_index.get(key).copied()))
            .collect()
    };
    let snap_ids = |v: &Value| {
        master::rows(v)
            .iter()
            .filter_map(|v| v.as_str().and_then(|key| snap_index.get(key).copied()))
            .collect()
    };
    let mut bindings = std::collections::HashMap::new();
    for binding in master::rows(&input["bindings"]) {
        let Some(member) = binding[0]
            .as_str()
            .and_then(|key| member_index.get(key).copied())
        else {
            continue;
        };
        if binding[1].is_null() {
            bindings.insert(member, -1);
        } else if let Some(snap) = binding[1]
            .as_str()
            .and_then(|key| snap_index.get(key).copied())
        {
            bindings.insert(member, snap as i32);
        }
    }
    crate::search::Constraints {
        required_members: member_ids(&input["requiredMembers"]),
        excluded_members: member_ids(&input["excludedMembers"]),
        required_snaps: snap_ids(&input["requiredSnaps"]),
        excluded_snaps: snap_ids(&input["excludedSnaps"]),
        leader: if input["leader"].is_null() {
            None
        } else {
            Some(
                input["leader"]
                    .as_str()
                    .and_then(|key| member_index.get(key).copied())
                    .map(|i| i as i32)
                    .unwrap_or(-1),
            )
        },
        bindings,
        no_snaps: input["noSnaps"] == true,
        min_bonus: input["minBonusPercent"]
            .as_f64()
            .map(|v| crate::num::round(v * 100.0)),
        max_bonus: None,
    }
}
impl Engine {
    pub fn search_power(
        &self,
        request: &Value,
        started: f64,
        now: &dyn Fn() -> f64,
    ) -> Result<Value, String> {
        let member_inputs: Vec<MemberInput> =
            serde_json::from_value(request["members"].clone()).map_err(|e| e.to_string())?;
        let snap_inputs: Vec<SnapInput> =
            serde_json::from_value(request["snaps"].clone()).map_err(|e| e.to_string())?;
        let cards = power::resolve_box(
            &self.master,
            &member_inputs,
            &snap_inputs,
            request["unknownPolicy"] != "min",
        );
        let player = power::player_state(&self.master, &request["player"]);
        let song = &request["goal"]["song"];
        let challenge = request["goal"]["challengeEventId"]
            .as_i64()
            .map(|id| id as i32);
        let music = if song.is_null() {
            None
        } else {
            Some(power::music_view(
                &self.master,
                master::int(&song["songId"]),
                challenge,
            )?)
        };
        let effects = challenge
            .and_then(|id| self.master.events.get(&id))
            .map(|e| e.effects.as_slice())
            .unwrap_or_default();
        let member_pct = |member: &power::Member| power::member_event_percent(effects, member, 2);
        let snap_pct = |snap: &power::Snap| power::snap_event_percent(effects, snap, 2);
        let constraints = constraints(&request["constraints"], &cards.members, &cards.snaps);
        let k = master::number(&request["k"], 1.0).max(1.0) as usize;
        let deadline = request["deadline"].as_f64().unwrap_or_else(|| {
            request["timeLimitMs"]
                .as_f64()
                .map(|ms| started + ms)
                .unwrap_or(f64::INFINITY)
        });
        let shard = if request["shard"].is_null() {
            None
        } else {
            Some((
                master::int(&request["shard"]["index"]) as usize,
                master::int(&request["shard"]["count"]) as usize,
            ))
        };
        let mut objective = PowerObjective;
        let generation = crate::cancellation_generation();
        let output = crate::search::search(crate::search::Input {
            master: &self.master,
            player: &player,
            members: &cards.members,
            snaps: &cards.snaps,
            music: music.as_ref(),
            objective: &mut objective,
            constraints,
            k,
            deadline,
            now,
            seeds: Vec::new(),
            shard,
            owns: None,
            floor: f64::NEG_INFINITY,
            floor_exclusive: false,
            streaming: true,
            disable_snap_dominance: false,
            member_power_percent: Some(&member_pct),
            snap_power_percent: Some(&snap_pct),
            on_candidate: None,
            progress: None,
        })?;
        let hits:Vec<_>=output.hits.iter().map(|hit|json!({"song":song,"members":hit.team.members.iter().map(|&i|cards.members[i].key.clone()).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(cards.snaps[j as usize].key.clone())}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key})).collect();
        Ok(
            json!({"results":[{"song":song,"hits":hits,"proven":output.proven,"bound":output.bound,"stats":output.stats}],"overall":hits,"unknownCards":cards.unknown,"elapsedMs":now()-started,"stopReason":if output.proven{None}else if generation!=crate::cancellation_generation(){Some("cancelled")}else{Some("budget")}}),
        )
    }
}

impl Engine {
    pub fn search_score(
        &self,
        request: &Value,
        started: f64,
        now: &dyn Fn() -> f64,
    ) -> Result<Value, String> {
        use crate::{
            objectives::{Criterion, LiveCore, LiveObjective},
            search,
        };
        let members: Vec<MemberInput> =
            serde_json::from_value(request["members"].clone()).map_err(|e| e.to_string())?;
        let snaps: Vec<SnapInput> =
            serde_json::from_value(request["snaps"].clone()).map_err(|e| e.to_string())?;
        let cards = power::resolve_box(
            &self.master,
            &members,
            &snaps,
            request["unknownPolicy"] != "min",
        );
        let player = power::player_state(&self.master, &request["player"]);
        let constraints = constraints(&request["constraints"], &cards.members, &cards.snaps);
        let k = master::number(&request["k"], 1.0).max(1.0) as usize;
        let deadline = request["deadline"].as_f64().unwrap_or_else(|| {
            request["timeLimitMs"]
                .as_f64()
                .map(|ms| started + ms)
                .unwrap_or(f64::INFINITY)
        });
        let generation = crate::cancellation_generation();
        let goal = &request["goal"];
        let songs = master::rows(&goal["songs"]);
        let mut results = Vec::new();
        let mut overall = Vec::new();
        let shard = if request["shard"].is_null() {
            None
        } else {
            Some((
                master::int(&request["shard"]["index"]) as usize,
                master::int(&request["shard"]["count"]) as usize,
            ))
        };
        for (index, song) in songs.iter().enumerate() {
            let done = master::rows(&request["completed"])
                .iter()
                .find(|row| row["proven"] == true && row["song"] == **song)
                .copied();
            if let Some(done) = done {
                results.push(done.clone());
                overall.extend(master::rows(&done["hits"]).iter().map(|v| (*v).clone()));
                continue;
            }
            if now() >= deadline || generation != crate::cancellation_generation() {
                for pending in &songs[index..] {
                    results.push(json!({"song":pending,"hits":[],"proven":false,"bound":null,"stats":search::Stats::default()}));
                }
                break;
            }
            let song_id = master::int(&song["songId"]);
            let difficulty = master::int(&song["difficulty"]);
            let chart = self
                .charts
                .get(&(song_id, difficulty))
                .ok_or_else(|| format!("missing-chart:{song_id}:{difficulty}"))?;
            let challenge = goal["challengeEventId"].as_i64().map(|id| id as i32);
            let music = power::music_view(&self.master, song_id, challenge)?;
            let effects = challenge
                .and_then(|id| self.master.events.get(&id))
                .map(|e| e.effects.as_slice())
                .unwrap_or_default();
            let member_pct = |m: &power::Member| power::member_event_percent(effects, m, 2);
            let snap_pct = |s: &power::Snap| power::snap_event_percent(effects, s, 2);
            let mut objective = LiveObjective {
                core: LiveCore::new(
                    &self.master,
                    chart,
                    &cards.members,
                    &cards.snaps,
                    &goal["play"],
                ),
                criterion: Criterion::from_value(&goal["criterion"]),
            };
            let song_deadline =
                (now() + (deadline - now()).max(0.0) / (songs.len() - index) as f64).min(deadline);
            let mut seeds: Vec<_> = master::rows(&request["seeds"])
                .iter()
                .filter(|hit| hit["song"] == **song)
                .filter_map(|hit| {
                    let members = master::rows(&hit["members"])
                        .iter()
                        .map(|v| {
                            cards
                                .members
                                .iter()
                                .position(|m| v.as_str() == Some(&m.key))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    let snaps = master::rows(&hit["snaps"])
                        .iter()
                        .map(|v| {
                            if v.is_null() {
                                Some(-1)
                            } else {
                                cards
                                    .snaps
                                    .iter()
                                    .position(|s| v.as_str() == Some(&s.key))
                                    .map(|i| i as i32)
                            }
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some(search::Team { members, snaps })
                })
                .collect();
            if objective.core.drains {
                let mut ap = LiveObjective {
                    core: LiveCore::new(
                        &self.master,
                        chart,
                        &cards.members,
                        &cards.snaps,
                        &Value::Null,
                    ),
                    criterion: objective.criterion,
                };
                let probe = search::search(search::Input {
                    master: &self.master,
                    player: &player,
                    members: &cards.members,
                    snaps: &cards.snaps,
                    music: Some(&music),
                    objective: &mut ap,
                    constraints: constraints.clone(),
                    k: 20.max(k * 3),
                    deadline: (now() + 150.0_f64.min((song_deadline - now()).max(0.0) / 4.0))
                        .min(song_deadline),
                    now,
                    seeds: seeds.clone(),
                    shard: None,
                    owns: None,
                    floor: f64::NEG_INFINITY,
                    floor_exclusive: false,
                    streaming: true,
                    disable_snap_dominance: false,
                    member_power_percent: Some(&member_pct),
                    snap_power_percent: Some(&snap_pct),
                    on_candidate: None,
                    progress: None,
                })?;
                seeds.extend(probe.hits.into_iter().map(|hit| hit.team));
            }
            if shard.is_some_and(|(_, count)| count > 1) {
                let probe = search::search(search::Input {
                    master: &self.master,
                    player: &player,
                    members: &cards.members,
                    snaps: &cards.snaps,
                    music: Some(&music),
                    objective: &mut objective,
                    constraints: constraints.clone(),
                    k,
                    deadline: (now() + 300.0).min(song_deadline),
                    now,
                    seeds: seeds.clone(),
                    shard: None,
                    owns: None,
                    floor: f64::NEG_INFINITY,
                    floor_exclusive: false,
                    streaming: true,
                    disable_snap_dominance: false,
                    member_power_percent: Some(&member_pct),
                    snap_power_percent: Some(&snap_pct),
                    on_candidate: None,
                    progress: None,
                })?;
                seeds.extend(probe.hits.into_iter().map(|hit| hit.team));
            }
            let output = search::search(search::Input {
                master: &self.master,
                player: &player,
                members: &cards.members,
                snaps: &cards.snaps,
                music: Some(&music),
                objective: &mut objective,
                constraints: constraints.clone(),
                k,
                deadline: song_deadline,
                now,
                seeds,
                shard,
                owns: None,
                floor: f64::NEG_INFINITY,
                floor_exclusive: false,
                streaming: true,
                disable_snap_dominance: false,
                member_power_percent: Some(&member_pct),
                snap_power_percent: Some(&snap_pct),
                on_candidate: None,
                progress: None,
            })?;
            let hits:Vec<_>=output.hits.iter().map(|hit|{
                let scores=&hit.detail["scores"];let values:Vec<_>=master::rows(&scores["scores"]).iter().map(|v|master::number(v,0.0)).collect();let mut sorted=values.clone();sorted.sort_by(f64::total_cmp);let mut ranks:BTreeMap<i32,i32>=BTreeMap::new();for &value in &values{*ranks.entry(chart::score_rank(chart,value)).or_default()+=1;}
                let keys=|order:usize|live::orders()[order].map(|slot|cards.members[hit.team.members[slot]].key.clone());
                json!({"song":song,"members":hit.team.members.iter().map(|&i|cards.members[i].key.clone()).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(cards.snaps[j as usize].key.clone())}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key,"score":{"mean":scores["mean"],"min":scores["min"],"max":scores["max"],"median":(sorted[59]+sorted[60])/2.0,"worstOrder":keys(master::int(&scores["minOrder"]) as usize),"bestOrder":keys(master::int(&scores["maxOrder"]) as usize),"orders":values.iter().map(|v|crate::num::round(*v)).collect::<Vec<_>>(),"ranks":ranks}})
            }).collect();
            overall.extend(hits.iter().cloned());
            results.push(json!({"song":song,"hits":hits,"proven":output.proven,"bound":output.bound,"stats":output.stats}));
        }
        overall.sort_by(|a, b| {
            master::number(&b["key"], 0.0)
                .total_cmp(&master::number(&a["key"], 0.0))
                .then_with(|| {
                    master::int(&a["song"]["songId"]).cmp(&master::int(&b["song"]["songId"]))
                })
                .then_with(|| {
                    master::int(&a["song"]["difficulty"])
                        .cmp(&master::int(&b["song"]["difficulty"]))
                })
                .then_with(|| {
                    master::number(&b["score"]["mean"], 0.0)
                        .total_cmp(&master::number(&a["score"]["mean"], 0.0))
                })
        });
        overall.truncate(k);
        Ok(
            json!({"results":results,"overall":overall,"unknownCards":cards.unknown,"elapsedMs":now()-started,"stopReason":if generation!=crate::cancellation_generation(){Some("cancelled")}else if now()>=deadline{Some("budget")}else{None}}),
        )
    }
}

impl Engine {
    pub fn full_orders(
        &self,
        request: &Value,
        team: &Value,
        song: &Value,
        source: &crate::chart::Source,
    ) -> Result<Value, String> {
        use crate::full::{
            conditions::Performer,
            model::{Model, Params},
            play::{self, Accuracy, Setup},
        };
        let inputs: Vec<MemberInput> =
            serde_json::from_value(request["members"].clone()).map_err(|e| e.to_string())?;
        let snaps: Vec<SnapInput> =
            serde_json::from_value(request["snaps"].clone()).map_err(|e| e.to_string())?;
        let cards = power::resolve_box(
            &self.master,
            &inputs,
            &snaps,
            request["unknownPolicy"] != "min",
        );
        let members = master::rows(&team["members"])
            .iter()
            .map(|key| {
                cards
                    .members
                    .iter()
                    .find(|m| key.as_str() == Some(&m.key))
                    .cloned()
                    .ok_or_else(|| format!("unknown member:{key}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let snaps = master::rows(&team["snaps"])
            .iter()
            .map(|key| {
                if key.is_null() {
                    Ok(None)
                } else {
                    cards
                        .snaps
                        .iter()
                        .find(|s| key.as_str() == Some(&s.key))
                        .cloned()
                        .map(Some)
                        .ok_or_else(|| format!("unknown snap:{key}"))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if members.len() != 5 || snaps.len() != 5 {
            return Err("a team requires five slots".into());
        }
        let song_id = master::int(&song["songId"]);
        let difficulty = master::int(&song["difficulty"]);
        let item = self.master.songs.get(&song_id).ok_or("unknown song")?;
        let level = item
            .difficulties
            .iter()
            .find(|d| d.difficulty == difficulty)
            .ok_or("unknown difficulty")?
            .play_level;
        let player = power::player_state(&self.master, &request["player"]);
        let music = power::music_view(&self.master, song_id, None)?;
        let power = power::team_power(
            &self.master,
            &player,
            &members,
            &snaps,
            0,
            Some(&music),
            &[],
        )?;
        let deck: Vec<_> = members
            .iter()
            .zip(&snaps)
            .map(|(m, s)| Performer::from_member(&self.master, m, s.as_ref()))
            .collect();
        let chart = play::compile(&self.master, source, level)?;
        let setup = Setup {
            fevers: chart.fevers.clone(),
            missions: item.missions.clone(),
        };
        if setup
            .missions
            .iter()
            .take(setup.fevers.len())
            .any(|&m| m == 2)
        {
            return Err("full Gekisou scores need a lottery-free chart".into());
        }
        let accuracy: Accuracy = serde_json::from_value(request["goal"]["accuracy"].clone())
            .map_err(|e| e.to_string())?;
        let play = play::judgement_stream(&self.master, &chart, Some(&setup), accuracy)?;
        let params = Params {
            music_type: music.music_type,
            power: power.total as i32,
            level,
            converted: chart.converted,
            music_length: chart.last_timing + 1000,
            assist: 1.0,
        };
        let mut scores = Vec::with_capacity(120);
        let generation = crate::cancellation_generation();
        for order in live::orders() {
            if generation != crate::cancellation_generation() {
                return Err("cancelled".into());
            }
            let ordered: Vec<_> = order.iter().map(|&slot| deck[slot].clone()).collect();
            let mut model = Model::new(
                &self.master,
                &ordered,
                &chart.notes,
                &chart.events,
                params.clone(),
                Some(&setup),
                (0, false),
            )?;
            for (i, frame) in play.frames.iter().enumerate() {
                model.frame(frame.time_ms, &frame.judged, play.dt[i])?;
            }
            scores.push(model.score.score.score as f64);
        }
        let mut sorted = scores.clone();
        sorted.sort_by(f64::total_cmp);
        let total = sorted.iter().fold(0.0, |sum, v| sum + v);
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let (mut min_order, mut max_order) = (0, 0);
        for (i, &value) in scores.iter().enumerate() {
            if value < min {
                min = value;
                min_order = i;
            }
            if value > max {
                max = value;
                max_order = i;
            }
        }
        Ok(
            json!({"power":power.total,"scores":scores,"mean":total/120.0,"min":min,"max":max,"minOrder":min_order,"maxOrder":max_order}),
        )
    }
}

impl Engine {
    pub fn evaluate_gekiso(
        &self,
        request: &Value,
        team: &Value,
        song: &Value,
        source: &crate::chart::Source,
    ) -> Result<Value, String> {
        use crate::{
            full::{conditions::Performer, play::Accuracy},
            gekiso,
        };
        let inputs: Vec<MemberInput> =
            serde_json::from_value(request["members"].clone()).map_err(|e| e.to_string())?;
        let snap_inputs: Vec<SnapInput> =
            serde_json::from_value(request["snaps"].clone()).map_err(|e| e.to_string())?;
        let cards = power::resolve_box(
            &self.master,
            &inputs,
            &snap_inputs,
            request["unknownPolicy"] != "min",
        );
        let members = master::rows(&team["members"])
            .iter()
            .map(|key| {
                cards
                    .members
                    .iter()
                    .find(|m| key.as_str() == Some(&m.key))
                    .cloned()
                    .ok_or_else(|| format!("unknown member:{key}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let snaps = master::rows(&team["snaps"])
            .iter()
            .map(|key| {
                if key.is_null() {
                    Ok(None)
                } else {
                    cards
                        .snaps
                        .iter()
                        .find(|s| key.as_str() == Some(&s.key))
                        .cloned()
                        .map(Some)
                        .ok_or_else(|| format!("unknown snap:{key}"))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if members.len() != 5 || snaps.len() != 5 {
            return Err("a team requires five slots".into());
        }
        let song_id = master::int(&song["songId"]);
        let difficulty = master::int(&song["difficulty"]);
        let item = self.master.songs.get(&song_id).ok_or("unknown song")?;
        let level = item
            .difficulties
            .iter()
            .find(|d| d.difficulty == difficulty)
            .ok_or("unknown difficulty")?
            .play_level;
        let player = power::player_state(&self.master, &request["player"]);
        let music = power::music_view(&self.master, song_id, None)?;
        let power = power::team_power(
            &self.master,
            &player,
            &members,
            &snaps,
            0,
            Some(&music),
            &[],
        )?;
        let deck: Vec<_> = members
            .iter()
            .zip(&snaps)
            .map(|(m, s)| Performer::from_member(&self.master, m, s.as_ref()))
            .collect();
        let accuracy: Accuracy = serde_json::from_value(request["goal"]["accuracy"].clone())
            .map_err(|e| e.to_string())?;
        let mut chart = gekiso::Chart::new(
            &self.master,
            source,
            level,
            item.missions.clone(),
            accuracy,
            master::number(&request["goal"]["rank"], 1.0) as i32,
        )?;
        let count = master::int(&request["goal"]["seeds"]).max(0);
        let seeds = if count > 0 {
            (0..count).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let ctx = gekiso::context(&self.master, &mut chart, &deck, &seeds)?;
        let mut unsupported = Vec::new();
        let slots: Vec<_> = members
            .iter()
            .zip(&snaps)
            .map(|(m, s)| skills::resolve(&self.master, m, s.as_ref(), &mut unsupported))
            .collect();
        let scores =
            gekiso::score_orders(&self.master, &chart, &ctx, power.total as i32, &slots, true)?;
        serde_json::to_value(scores).map_err(|e| e.to_string())
    }
}

impl Engine {
    pub fn search_gekiso(
        &self,
        request: &Value,
        source: &crate::chart::Source,
        started: f64,
        now: &dyn Fn() -> f64,
    ) -> Result<Value, String> {
        use crate::{full::play::Accuracy, gekiso, gekiso_objective, objectives::Criterion};
        let inputs: Vec<MemberInput> =
            serde_json::from_value(request["members"].clone()).map_err(|e| e.to_string())?;
        let snap_inputs: Vec<SnapInput> =
            serde_json::from_value(request["snaps"].clone()).map_err(|e| e.to_string())?;
        let cards = power::resolve_box(
            &self.master,
            &inputs,
            &snap_inputs,
            request["unknownPolicy"] != "min",
        );
        let player = power::player_state(&self.master, &request["player"]);
        let goal = &request["goal"];
        let song = master::rows(&goal["songs"])
            .first()
            .copied()
            .ok_or("no Gekiso song")?;
        let song_id = master::int(&song["songId"]);
        let difficulty = master::int(&song["difficulty"]);
        let item = self.master.songs.get(&song_id).ok_or("unknown song")?;
        let level = item
            .difficulties
            .iter()
            .find(|d| d.difficulty == difficulty)
            .ok_or("unknown difficulty")?
            .play_level;
        let music = power::music_view(&self.master, song_id, None)?;
        let accuracy: Accuracy =
            serde_json::from_value(goal["accuracy"].clone()).map_err(|e| e.to_string())?;
        let mut chart = gekiso::Chart::new(
            &self.master,
            source,
            level,
            item.missions.clone(),
            accuracy,
            master::number(&goal["rank"], 1.0) as i32,
        )?;
        let seed_count = master::int(&goal["seeds"]).max(0);
        let seeds: Vec<i32> = (0..seed_count).collect();
        let criterion = Criterion::from_value(&goal["criterion"]);
        let constraints = constraints(&request["constraints"], &cards.members, &cards.snaps);
        let k = master::number(&request["k"], 1.0).max(1.0) as usize;
        let deadline = request["deadline"].as_f64().unwrap_or_else(|| {
            request["timeLimitMs"]
                .as_f64()
                .map(|ms| started + ms)
                .unwrap_or(f64::INFINITY)
        });
        let shard = if request["shard"].is_null() {
            None
        } else {
            Some((
                master::int(&request["shard"]["index"]) as usize,
                master::int(&request["shard"]["count"]) as usize,
            ))
        };
        let exact_ranking = !chart.luck && seed_count <= 1 && request["seedOnly"] != true;
        let mut extra = if exact_ranking { 4 } else { 0 };
        let margin = |key: f64| key.abs().max(1.0) * 3e-5;
        let run = |chart: &mut gekiso::Chart, extra: usize| {
            gekiso_objective::search(gekiso_objective::Input {
                env: gekiso_objective::Environment {
                    master: &self.master,
                    player: &player,
                    members: &cards.members,
                    snaps: &cards.snaps,
                    music: Some(&music),
                    constraints: constraints.clone(),
                    now,
                },
                chart,
                seeds: seeds.clone(),
                criterion,
                k: k + extra,
                deadline,
                initial: Vec::new(),
                seed_only: request["seedOnly"] == true,
                seeded: request["seeded"] == true,
                shard,
                floor: f64::NEG_INFINITY,
                on_candidate: None,
                progress: None,
            })
        };
        let mut output = run(&mut chart, extra)?;
        if exact_ranking {
            while output.proven
                && output.hits.len() == k + extra
                && output.hits.len() >= k
                && output.hits.last().unwrap().key
                    >= output.hits[k - 1].key - 2.0 * margin(output.hits[k - 1].key)
            {
                extra *= 2;
                output = run(&mut chart, extra)?;
            }
            let kth = output
                .hits
                .get(k.min(output.hits.len()).saturating_sub(1))
                .map(|hit| hit.key)
                .unwrap_or(f64::NEG_INFINITY);
            let mut finalists = Vec::new();
            for mut hit in output
                .hits
                .into_iter()
                .filter(|hit| hit.key >= kth - 2.0 * margin(kth))
            {
                let team = json!({"members":hit.team.members.iter().map(|&i|&cards.members[i].key).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(&cards.snaps[j as usize].key)}).collect::<Vec<_>>()});
                let scores = self.full_orders(request, &team, song, source)?;
                hit.key = match criterion {
                    Criterion::Mean => master::number(&scores["mean"], 0.0),
                    Criterion::Min => master::number(&scores["min"], 0.0),
                    Criterion::Max => master::number(&scores["max"], 0.0),
                };
                hit.detail["scores"] = scores;
                finalists.push(hit);
            }
            finalists.sort_by(|a, b| b.key.total_cmp(&a.key));
            finalists.truncate(k);
            output.hits = finalists;
        }

        let compiled = crate::chart::compile(
            &self.master,
            item,
            item.difficulties
                .iter()
                .find(|d| d.difficulty == difficulty)
                .unwrap(),
            source,
        );
        let hits:Vec<_>=output.hits.iter().map(|hit|{let scores=&hit.detail["scores"];let values:Vec<_>=master::rows(&scores["scores"]).iter().map(|v|master::number(v,0.0)).collect();let mut sorted=values.clone();sorted.sort_by(f64::total_cmp);let mut ranks:BTreeMap<i32,i32>=BTreeMap::new();for &value in &values{*ranks.entry(crate::chart::battle_rank(&compiled,value)).or_default()+=1;}let keys=|order:usize|live::orders()[order].map(|slot|cards.members[hit.team.members[slot]].key.clone());json!({"song":song,"members":hit.team.members.iter().map(|&i|cards.members[i].key.clone()).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(cards.snaps[j as usize].key.clone())}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key,"score":{"mean":scores["mean"],"min":scores["min"],"max":scores["max"],"median":(sorted[59]+sorted[60])/2.0,"worstOrder":keys(master::int(&scores["minOrder"]) as usize),"bestOrder":keys(master::int(&scores["maxOrder"]) as usize),"orders":values.iter().map(|v|crate::num::round(*v)).collect::<Vec<_>>(),"ranks":ranks}})}).collect();
        Ok(
            json!({"results":[{"song":song,"hits":hits,"proven":output.proven,"bound":output.bound,"stats":output.stats}],"overall":hits,"unknownCards":cards.unknown,"elapsedMs":now()-started}),
        )
    }
}
