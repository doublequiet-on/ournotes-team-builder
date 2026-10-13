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
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
pub struct Engine {
    pub master: Master,
    pub charts: BTreeMap<(i32, i32), Chart>,
    pub sources: BTreeMap<(i32, i32), Source>,
    chart_order: VecDeque<(i32, i32)>,
    live_cache: RefCell<Option<(String, crate::objectives::LiveCore)>>,
    gekiso_cache: RefCell<Option<(String, crate::gekiso::Chart)>>,
}
impl Engine {
    pub fn compile(data: &Value) -> Result<Self, String> {
        Ok(Self::from_master(master::compile(data)?))
    }
    pub fn from_master(master: Master) -> Self {
        Self {
            master,
            charts: BTreeMap::new(),
            sources: BTreeMap::new(),
            chart_order: VecDeque::new(),
            live_cache: RefCell::new(None),
            gekiso_cache: RefCell::new(None),
        }
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
        let key = (song_id, difficulty);
        self.live_cache.replace(None);
        self.gekiso_cache.replace(None);
        self.chart_order.retain(|&old| old != key);
        if self.charts.len() >= 4 && !self.charts.contains_key(&key) {
            if let Some(key) = self.chart_order.pop_front() {
                self.charts.remove(&key);
                self.sources.remove(&key);
            }
        }
        self.chart_order.push_back(key);
        self.charts.insert(
            (song_id, difficulty),
            chart::compile(&self.master, song, level, &source),
        );
        self.sources.insert(key, source);
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
        let leader = team["leader"]
            .as_str()
            .unwrap_or_else(|| keys[0].as_str().unwrap_or(""));
        let leader_index = keys
            .iter()
            .position(|key| key.as_str() == Some(leader))
            .ok_or("unknown-team-member")?;
        let indices: Vec<_> = std::iter::once(leader_index)
            .chain((0..5).filter(|&i| i != leader_index))
            .collect();
        let members = indices
            .iter()
            .map(|&i| {
                let key = &keys[i];
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
        let snaps = indices
            .iter()
            .map(|&i| {
                let key = &snap_keys[i];
                if key.is_null() {
                    None
                } else {
                    cards
                        .snaps
                        .iter()
                        .find(|s| key.as_str() == Some(s.key.as_str()))
                        .cloned()
                }
            })
            .collect::<Vec<_>>();
        crate::progress::keys(&cards.members, &cards.snaps);
        let player = power::player_state(&self.master, &request["player"]);
        let challenge_event = if request["event"]["route"] == "challenge" {
            request["event"]["eventId"].as_i64()
        } else {
            request["challengeEventId"]
                .as_i64()
                .or_else(|| request["goal"]["challengeEventId"].as_i64())
        }
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
        let event = if request["event"].is_null() {
            Value::Null
        } else {
            let input = &request["event"];
            let kind = match input["route"].as_str() {
                Some("challenge") => crate::event::RouteKind::Challenge,
                Some("skip") => crate::event::RouteKind::Skip,
                _ => crate::event::RouteKind::Live,
            };
            let route = crate::event::Route::new(
                &self.master,
                master::int(&input["eventId"]),
                kind,
                master::int(&input["consumption"]),
            )?;
            let bonus = |kind| {
                members
                    .iter()
                    .map(|m| power::member_event_percent(&route.effects, m, kind) as f64)
                    .sum::<f64>()
                    + snaps
                        .iter()
                        .flatten()
                        .map(|s| power::snap_event_percent(&route.effects, s, kind) as f64)
                        .sum::<f64>()
            };
            let point_bonus = bonus(0);
            let item_bonus = bonus(1);
            let mut points = 0.0;
            let mut items = 0.0;
            for &score in &scores.scores {
                let rank = chart::score_rank(chart, score);
                points += route.points(point_bonus, rank);
                items += route.items(item_bonus, rank);
            }
            json!({"points":points/120.0,"items":items/120.0,"bonusPercent":point_bonus/100.0,"itemBonusPercent":item_bonus/100.0})
        };
        Ok(
            json!({"song":{"songId":song_id,"difficulty":difficulty},"power":power.total,"slotPowers":power.slots,"leaderPercents":power.leader_percents,"unknownCards":cards.unknown,"unsupported":unsupported,"skipScore":chart::skip_score(&self.master,chart,power.total as i32),"event":event,"score":{"mean":scores.mean,"min":scores.min,"max":scores.max,"median":(sorted[59]+sorted[60])/2.0,"worstOrder":keys_of(scores.min_order),"bestOrder":keys_of(scores.max_order),"orders":scores.scores.iter().map(|v|crate::num::round(*v)).collect::<Vec<_>>(),"ranks":ranks}}),
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
        crate::progress::keys(&cards.members, &cards.snaps);
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
        let mut objective: Box<dyn crate::search::Objective> =
            if request["goal"]["kind"] == "potential" {
                Box::new(crate::objectives::Potential::new(
                    &self.master,
                    &cards.members,
                    &cards.snaps,
                    master::number(&request["goal"]["windowSeconds"], 120.0),
                ))
            } else {
                Box::new(PowerObjective)
            };
        let generation = crate::cancellation_generation();
        let output = crate::search::search(crate::search::Input {
            master: &self.master,
            player: &player,
            members: &cards.members,
            snaps: &cards.snaps,
            music: music.as_ref(),
            objective: objective.as_mut(),
            constraints,
            k,
            deadline,
            now,
            seeds: seed_teams(request, song, &cards.members, &cards.snaps),
            shard,
            owns: None,
            floor: f64::NEG_INFINITY,
            floor_exclusive: false,
            streaming: true,
            disable_snap_dominance: false,
            member_power_percent: Some(&member_pct),
            snap_power_percent: Some(&snap_pct),
            on_candidate: Some(&mut crate::progress::emit),
            progress: None,
        })?;
        let hits:Vec<_>=output.hits.iter().map(|hit|json!({"song":song,"members":hit.team.members.iter().map(|&i|cards.members[i].key.clone()).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(cards.snaps[j as usize].key.clone())}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key,"potential":if hit.detail["potential"].is_null(){Value::Null}else{json!({"value":hit.detail["potential"],"area":hit.detail["area"]})}})).collect();
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
        crate::progress::keys(&cards.members, &cards.snaps);
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
            crate::progress::song(song);
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
            let cache_key = json!([
                song,
                request["members"],
                request["snaps"],
                request["unknownPolicy"],
                goal["play"]
            ])
            .to_string();
            let core = self
                .live_cache
                .borrow_mut()
                .take()
                .filter(|(key, _)| key == &cache_key)
                .map(|(_, core)| core)
                .unwrap_or_else(|| {
                    LiveCore::new(
                        &self.master,
                        chart,
                        &cards.members,
                        &cards.snaps,
                        &goal["play"],
                    )
                });
            let mut objective = LiveObjective {
                core,
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
                on_candidate: Some(&mut crate::progress::emit),
                progress: None,
            })?;
            let hits:Vec<_>=output.hits.iter().map(|hit|{
                let scores=&hit.detail["scores"];let values:Vec<_>=master::rows(&scores["scores"]).iter().map(|v|master::number(v,0.0)).collect();let mut sorted=values.clone();sorted.sort_by(f64::total_cmp);let mut ranks:BTreeMap<i32,i32>=BTreeMap::new();for &value in &values{*ranks.entry(chart::score_rank(chart,value)).or_default()+=1;}
                let keys=|order:usize|live::orders()[order].map(|slot|cards.members[hit.team.members[slot]].key.clone());
                json!({"song":song,"members":hit.team.members.iter().map(|&i|cards.members[i].key.clone()).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(cards.snaps[j as usize].key.clone())}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key,"score":{"mean":scores["mean"],"min":scores["min"],"max":scores["max"],"median":(sorted[59]+sorted[60])/2.0,"worstOrder":keys(master::int(&scores["minOrder"]) as usize),"bestOrder":keys(master::int(&scores["maxOrder"]) as usize),"orders":values.iter().map(|v|crate::num::round(*v)).collect::<Vec<_>>(),"ranks":ranks}})
            }).collect();
            self.live_cache.replace(Some((cache_key, objective.core)));
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
        crate::progress::keys(&cards.members, &cards.snaps);
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
        crate::progress::keys(&cards.members, &cards.snaps);
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
        crate::progress::keys(&cards.members, &cards.snaps);
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
        let cache_key = json!([
            song,
            request["members"],
            request["snaps"],
            request["player"],
            request["unknownPolicy"],
            request["constraints"],
            goal["accuracy"],
            goal["rank"]
        ])
        .to_string();
        let mut chart = if let Some((_, chart)) = self
            .gekiso_cache
            .borrow_mut()
            .take()
            .filter(|(key, _)| key == &cache_key)
        {
            chart
        } else {
            gekiso::Chart::new(
                &self.master,
                source,
                level,
                item.missions.clone(),
                accuracy,
                master::number(&goal["rank"], 1.0) as i32,
            )?
        };
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
        let song_key = format!("{song_id}:{difficulty}");
        let known = request["floors"][song_key].as_f64();
        let floor = known
            .map(|value| {
                if exact_ranking {
                    value - 2.0 * margin(value)
                } else {
                    value
                }
            })
            .unwrap_or(f64::NEG_INFINITY);
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
                initial: seed_teams(request, song, &cards.members, &cards.snaps),
                seed_only: request["seedOnly"] == true,
                seeded: request["seeded"] == true,
                shard,
                floor,
                on_candidate: Some(&mut crate::progress::emit),
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
        self.gekiso_cache.replace(Some((cache_key, chart)));
        Ok(
            json!({"results":[{"song":song,"hits":hits,"proven":output.proven,"bound":output.bound,"stats":output.stats}],"overall":hits,"unknownCards":cards.unknown,"elapsedMs":now()-started}),
        )
    }
}

impl Engine {
    /// Ordinary activity rewards. Sampled Gekisou and strict PT validation are
    /// separate protocols and must be validated before entering this route.
    pub fn search_event(
        &self,
        request: &Value,
        started: f64,
        now: &dyn Fn() -> f64,
    ) -> Result<Value, String> {
        use crate::{
            event::{ChallengePoints, EventObjective, Route, RouteKind, ScoreTie},
            objectives::{Criterion, LiveCore, LiveObjective},
            search::{self, Objective},
        };
        let goal = &request["goal"];
        if !goal["gekiso"].is_null() {
            return Err("event-protocol-not-supported".into());
        }
        let pt = goal["ranking"] == "pt-only";
        if pt {
            let issues = crate::pt::validate(&self.master, request)?;
            if !issues.is_empty() {
                return Err(format!("pt-input:{}", issues.join(",")));
            }
        }
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
        crate::progress::keys(&cards.members, &cards.snaps);
        let player = power::player_state(&self.master, &request["player"]);
        let kind = match goal["route"].as_str() {
            Some("challenge") => RouteKind::Challenge,
            Some("skip") => RouteKind::Skip,
            _ => RouteKind::Live,
        };
        let route = Route::new(
            &self.master,
            master::int(&goal["eventId"]),
            kind,
            master::int(&goal["consumption"]),
        )?;
        let songs = if kind == RouteKind::Skip && master::rows(&goal["songs"]).is_empty() {
            vec![Value::Null]
        } else {
            master::rows(&goal["songs"]).into_iter().cloned().collect()
        };
        let k = master::number(&request["k"], 1.0).max(1.0) as usize;
        let deadline = request["deadline"].as_f64().unwrap_or_else(|| {
            request["timeLimitMs"]
                .as_f64()
                .map(|ms| started + ms)
                .unwrap_or(f64::INFINITY)
        });
        let primary = !goal["ranking"].is_null();
        let exchange = if goal["cpExchange"].is_null() {
            None
        } else {
            Some((
                master::number(&goal["cpExchange"]["numerator"], 0.0),
                master::number(&goal["cpExchange"]["denominator"], 1.0),
            ))
        };
        if exchange.is_some_and(|(_, d)| d <= 0.0) {
            return Err("invalid-cp-exchange".into());
        }
        let constraints = constraints(&request["constraints"], &cards.members, &cards.snaps);
        let generation = crate::cancellation_generation();
        let mut results = Vec::new();
        let mut overall = Vec::new();
        for song in songs {
            if now() >= deadline || generation != crate::cancellation_generation() {
                results.push(json!({"song":song,"hits":[],"proven":false,"bound":null,"stats":search::Stats::default()}));
                continue;
            }
            let chart = if song.is_null() {
                None
            } else {
                self.charts.get(&(
                    master::int(&song["songId"]),
                    master::int(&song["difficulty"]),
                ))
            };
            if kind != RouteKind::Skip && chart.is_none() {
                return Err("missing-chart".into());
            }
            if pt {
                crate::pt::validate_chart(
                    &self.master,
                    master::int(&song["songId"]),
                    master::int(&song["difficulty"]),
                    self.sources
                        .get(&(
                            master::int(&song["songId"]),
                            master::int(&song["difficulty"]),
                        ))
                        .ok_or("missing-chart")?,
                )?;
            }
            let challenge = if kind == RouteKind::Challenge {
                Some(master::int(&goal["eventId"]))
            } else {
                None
            };
            let music = if song.is_null() {
                None
            } else {
                Some(power::music_view(
                    &self.master,
                    master::int(&song["songId"]),
                    challenge,
                )?)
            };
            let member_pct = |m: &power::Member| {
                if kind == RouteKind::Challenge {
                    power::member_event_percent(&route.effects, m, 2)
                } else {
                    0
                }
            };
            let snap_pct = |s: &power::Snap| {
                if kind == RouteKind::Challenge {
                    power::snap_event_percent(&route.effects, s, 2)
                } else {
                    0
                }
            };
            let make_event = || {
                EventObjective::new(
                    &self.master,
                    &cards.members,
                    &cards.snaps,
                    chart,
                    route.clone(),
                    goal["measure"] != "items",
                    &goal["play"],
                    primary,
                    0.0,
                    exchange,
                    f64::INFINITY,
                )
            };
            let cp_measure =
                goal["measure"] == "challenge-points" && chart.is_some() && kind != RouteKind::Skip;
            let mut objective: Box<dyn Objective> = if cp_measure {
                Box::new(ChallengePoints {
                    mean: LiveObjective {
                        core: LiveCore::new(
                            &self.master,
                            chart.unwrap(),
                            &cards.members,
                            &cards.snaps,
                            &goal["play"],
                        ),
                        criterion: Criterion::Mean,
                    },
                    chart: chart.unwrap().clone(),
                    route: route.clone(),
                })
            } else {
                Box::new(make_event())
            };
            let shard = if request["shard"].is_null() {
                None
            } else {
                Some((
                    master::int(&request["shard"]["index"]) as usize,
                    master::int(&request["shard"]["count"]) as usize,
                ))
            };
            let run = |objective: &mut dyn Objective, k, seeds, floor, notify| {
                let mut publish = crate::progress::emit;
                search::search(search::Input {
                    master: &self.master,
                    player: &player,
                    members: &cards.members,
                    snaps: &cards.snaps,
                    music: music.as_ref(),
                    objective,
                    constraints: constraints.clone(),
                    k,
                    deadline,
                    now,
                    seeds,
                    shard,
                    owns: None,
                    floor,
                    floor_exclusive: false,
                    streaming: true,
                    disable_snap_dominance: false,
                    member_power_percent: Some(&member_pct),
                    snap_power_percent: Some(&snap_pct),
                    on_candidate: if notify { Some(&mut publish) } else { None },
                    progress: None,
                })
            };
            let mut output = run(
                objective.as_mut(),
                k,
                seed_teams(request, &song, &cards.members, &cards.snaps),
                f64::NEG_INFINITY,
                true,
            )?;
            if let Some(tie_chart) = chart.filter(|_| {
                !primary && !cp_measure && kind != RouteKind::Skip && !output.hits.is_empty()
            }) {
                let mut keys: Vec<_> = output.hits.iter().map(|h| h.key).collect();
                keys.sort_by(|a, b| b.total_cmp(a));
                keys.dedup();
                let mut ordered = Vec::<search::Hit>::new();
                for key in keys {
                    if ordered.len() >= k {
                        break;
                    }
                    let seeds = output
                        .hits
                        .iter()
                        .filter(|h| h.key == key)
                        .map(|h| h.team.clone())
                        .collect();
                    let excluded: Vec<_> = ordered.iter().map(|h| h.team.clone()).collect();
                    let mut tie = ScoreTie::new(
                        make_event(),
                        LiveObjective {
                            core: LiveCore::new(
                                &self.master,
                                tie_chart,
                                &cards.members,
                                &cards.snaps,
                                &goal["play"],
                            ),
                            criterion: Criterion::Mean,
                        },
                        key,
                        &excluded,
                    );
                    let reranked = run(&mut tie, k - ordered.len(), seeds, 0.0, false)?;
                    if !reranked.proven {
                        output.proven = false;
                        output.bound = Some(output.bound.unwrap_or(f64::NEG_INFINITY).max(key));
                    }
                    for mut hit in reranked.hits {
                        if hit.key < 0.0 {
                            continue;
                        }
                        hit.key = key;
                        hit.detail = objective
                            .exact(&hit.team, hit.power, &hit.slot_powers)?
                            .detail;
                        ordered.push(hit);
                    }
                }
                output.hits = ordered;
            }
            let hits:Vec<_>=output.hits.iter().map(|hit|{
                let d=&hit.detail;let mut value=json!({"song":song,"members":hit.team.members.iter().map(|&i|&cards.members[i].key).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(&cards.snaps[j as usize].key)}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key,"event":{"mean":master::number(&d["mean"],0.0)/exchange.map(|(_,d)|d).unwrap_or(1.0),"bonusPercent":master::number(&d["bonus"],0.0)/100.0,"challengePoints":master::number(&d["challengePoints"],0.0),"perPlayMin":master::number(&d["directMin"],master::rows(&d["perOrder"]).into_iter().map(|v|master::number(v,0.0)).reduce(f64::min).unwrap_or_else(||master::number(&d["mean"],0.0))),"perPlayMax":master::number(&d["directMax"],master::rows(&d["perOrder"]).into_iter().map(|v|master::number(v,0.0)).reduce(f64::max).unwrap_or_else(||master::number(&d["mean"],0.0)))}});
                if pt {value["event"]["rewardSum"]=d["rewardSum"].clone();value["event"]["comparisonSum"]=d["comparisonSum"].clone();value["event"]["orders"]=json!(120);value["event"]["directMean"]=json!(master::number(&d["rewardSum"],0.0)/120.0);value["event"]["convertedMean"]=json!(exchange.map(|(n,den)|master::number(&d["challengePoints"],0.0)*n/den).unwrap_or(0.0));}
                if let Some(chart)=chart.filter(|_|!d["scores"].is_null()){
                    let scores=&d["scores"];let values:Vec<_>=master::rows(&scores["scores"]).iter().map(|v|master::number(v,0.0)).collect();let mut sorted=values.clone();sorted.sort_by(f64::total_cmp);let mut ranks=BTreeMap::<i32,i32>::new();for &v in &values{*ranks.entry(chart::score_rank(chart,v)).or_default()+=1;}let keys=|order:usize|live::orders()[order].map(|slot|cards.members[hit.team.members[slot]].key.clone());value["score"]=json!({"mean":scores["mean"],"min":scores["min"],"max":scores["max"],"median":(sorted[59]+sorted[60])/2.0,"orders":values,"bestOrder":keys(master::int(&scores["maxOrder"]) as usize),"worstOrder":keys(master::int(&scores["minOrder"]) as usize),"ranks":ranks});
                }value
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
        });
        overall.truncate(k);
        Ok(
            json!({"results":results,"overall":overall,"unknownCards":cards.unknown,"elapsedMs":now()-started,"stopReason":if generation!=crate::cancellation_generation(){Some("cancelled")}else if now()>=deadline{Some("budget")}else{None}}),
        )
    }
}

impl Engine {
    /// Charts enter just before their song is evaluated, keeping a bounded
    /// release-bound cache even for requests containing the entire song list.
    pub fn run(
        &mut self,
        input: &Value,
        started: f64,
        now: &dyn Fn() -> f64,
    ) -> Result<Value, String> {
        let request = &input["request"];
        let kind = request["goal"]["kind"].as_str().ok_or("missing-goal")?;
        crate::progress::mode(&request["goal"]);
        let primary = kind == "event" && !request["goal"]["ranking"].is_null();
        let pt = kind == "event" && request["goal"]["ranking"] == "pt-only";
        if !request["cutoff"].is_null()
            && (!primary
                || master::number(&request["k"], 1.0) != 1.0
                || request["cutoff"]["contextKey"] != request["contextKey"]
                || !request["cutoff"]["key"]
                    .as_f64()
                    .is_some_and(f64::is_finite))
        {
            return Err("invalid-reward-cutoff".into());
        }
        if !request["rewardCutoff"].is_null()
            && (!pt
                || master::number(&request["k"], 1.0) != 1.0
                || !request["rewardCutoff"].as_f64().is_some_and(f64::is_finite))
        {
            return Err("invalid-reward-cutoff".into());
        }
        if pt {
            let issues = crate::pt::validate(&self.master, request)?;
            if !issues.is_empty() {
                return Err(format!("pt-input:{}", issues.join(",")));
            }
        }
        if kind == "plan" {
            return self.plan(input, started, now);
        }
        let songs = if kind == "power" || kind == "potential" {
            vec![request["goal"]["song"].clone()]
        } else {
            master::rows(&request["goal"]["songs"])
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        let songs = if songs.is_empty() && kind == "event" && request["goal"]["route"] == "skip" {
            vec![Value::Null]
        } else {
            songs
        };
        if !["power", "potential", "score", "gekiso", "event"].contains(&kind) {
            return Err(format!("unsupported-goal:{kind}"));
        }
        let deadline = request["deadline"].as_f64().unwrap_or_else(|| {
            request["timeLimitMs"]
                .as_f64()
                .map(|ms| started + ms)
                .unwrap_or(f64::INFINITY)
        });
        let generation = crate::cancellation_generation();
        let k = master::number(&request["k"], 1.0).max(1.0) as usize;
        let mut results = Vec::new();
        let mut overall = Vec::new();
        let mut unknown = Value::Null;
        for (index, song) in songs.iter().enumerate() {
            crate::progress::song(song);
            if let Some(done) = master::rows(&request["completed"])
                .into_iter()
                .find(|r| r["song"] == *song && r["proven"] == true)
            {
                results.push(done.clone());
                overall.extend(master::rows(&done["hits"]).into_iter().cloned());
                continue;
            }
            if now() >= deadline || generation != crate::cancellation_generation() {
                results.push(json!({"song":song,"hits":[],"proven":false,"bound":null,"stats":crate::search::Stats::default()}));
                continue;
            }
            let key = (
                master::int(&song["songId"]),
                master::int(&song["difficulty"]),
            );
            if let Some(entry) = master::rows(&input["charts"])
                .into_iter()
                .find(|r| r["song"] == *song)
            {
                let source = if entry["source"].is_null() {
                    crate::runtime_chart::convert(&entry["chart"])?
                } else {
                    serde_json::from_value(entry["source"].clone()).map_err(|e| e.to_string())?
                };
                self.insert_chart(key.0, key.1, source)?;
            }
            if self.charts.contains_key(&key) {
                self.chart_order.retain(|&old| old != key);
                self.chart_order.push_back(key);
            }
            crate::progress::chart(
                self.charts.get(&key),
                kind == "gekiso" || !request["goal"]["gekiso"].is_null(),
            );
            let mut single = request.clone();
            single["goal"]["songs"] = json!([song]);
            let current = now();
            let song_deadline = (current
                + (deadline - current).max(0.0) / (songs.len() - index) as f64)
                .min(deadline);
            if song_deadline.is_finite() {
                single["deadline"] = json!(song_deadline);
            }
            let mut response = match kind {
                "power" | "potential" => self.search_power(&single, started, now)?,
                "score" => self.search_score(&single, started, now)?,
                "event" => {
                    if single["goal"]["gekiso"].is_null() {
                        self.search_event(&single, started, now)?
                    } else {
                        self.search_rewards(&single, started, now)?
                    }
                }
                "gekiso" => self.search_gekiso(
                    &single,
                    self.sources.get(&key).ok_or("missing-chart")?,
                    started,
                    now,
                )?,
                _ => unreachable!(),
            };
            apply_reward_cutoff(&mut response, request, song);
            unknown = response["unknownCards"].clone();
            results.extend(master::rows(&response["results"]).into_iter().cloned());
            overall.extend(master::rows(&response["overall"]).into_iter().cloned());
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
        let complete = results.iter().all(|row| {
            row["proven"] == true || !row["exclusion"].is_null() || !row["excludedBelow"].is_null()
        });
        Ok(
            json!({"results":results,"overall":overall,"unknownCards":unknown,"cachedCharts":self.chart_order.iter().map(|(s,d)|format!("{s}:{d}")).collect::<Vec<_>>(),"elapsedMs":now()-started,"stopReason":if complete{Some("complete")}else if generation!=crate::cancellation_generation(){Some("cancelled")}else{Some("budget")}}),
        )
    }
}

fn seed_teams(
    request: &Value,
    song: &Value,
    members: &[power::Member],
    snaps: &[power::Snap],
) -> Vec<crate::search::Team> {
    master::rows(&request["seeds"])
        .into_iter()
        .filter(|h| h["song"] == *song)
        .filter_map(|hit| {
            let members = master::rows(&hit["members"])
                .into_iter()
                .map(|v| members.iter().position(|m| v.as_str() == Some(&m.key)))
                .collect::<Option<Vec<_>>>()?;
            let snaps = master::rows(&hit["snaps"])
                .into_iter()
                .map(|v| {
                    if v.is_null() {
                        Some(-1)
                    } else {
                        snaps
                            .iter()
                            .position(|s| v.as_str() == Some(&s.key))
                            .map(|i| i as i32)
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            if members.len() != 5 || snaps.len() != 5 {
                return None;
            }
            Some(crate::search::Team { members, snaps })
        })
        .collect()
}

impl Engine {
    pub fn search_rewards(
        &self,
        request: &Value,
        started: f64,
        now: &dyn Fn() -> f64,
    ) -> Result<Value, String> {
        use crate::{
            event::{BonusSurrogate, Route, RouteKind},
            full::play::Accuracy,
            rewards::RewardObjective,
            search,
        };
        let goal = &request["goal"];
        if goal["ranking"] != "pt-only" || goal["route"] != "live" {
            return Err("gekiso-reward-mode".into());
        }
        let issues = crate::pt::validate(&self.master, request)?;
        if !issues.is_empty() {
            return Err(format!("pt-input:{}", issues.join(",")));
        }
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
        let constraints = constraints(&request["constraints"], &cards.members, &cards.snaps);
        crate::progress::keys(&cards.members, &cards.snaps);
        let song = master::rows(&goal["songs"])
            .first()
            .copied()
            .ok_or("no Gekiso song")?;
        crate::progress::song(song);
        let id = master::int(&song["songId"]);
        let difficulty = master::int(&song["difficulty"]);
        let source = self.sources.get(&(id, difficulty)).ok_or("missing-chart")?;
        crate::pt::validate_chart(&self.master, id, difficulty, source)?;
        if source
            .enumeration
            .as_ref()
            .is_none_or(|rows| rows.is_empty())
            || source.fever_ms.len() != 3
        {
            return Err(format!("gekiso-chart-unsupported:{id}:{difficulty}"));
        }
        let chart = self.charts.get(&(id, difficulty)).ok_or("missing-chart")?;
        let music = power::music_view(&self.master, id, None)?;
        let route = Route::new(
            &self.master,
            master::int(&goal["eventId"]),
            RouteKind::Live,
            master::int(&goal["consumption"]),
        )?;
        let points = goal["measure"] != "items";
        let bonus_type = if points { 0 } else { 1 };
        let exchange = if goal["cpExchange"].is_null() {
            None
        } else {
            Some((
                master::number(&goal["cpExchange"]["numerator"], 0.0),
                master::number(&goal["cpExchange"]["denominator"], 1.0),
            ))
        };
        let accuracy = Accuracy {
            just: master::number(&goal["gekiso"]["just"], 1.0),
            ..Accuracy::default()
        };
        let gekiso = crate::gekiso::Chart::new(
            &self.master,
            source,
            chart.difficulty.play_level,
            chart.song.missions.clone(),
            accuracy,
            1,
        )?;
        let mut objective = RewardObjective::new(
            &self.master,
            &cards.members,
            &cards.snaps,
            chart,
            gekiso,
            route.clone(),
            points,
            master::int(&goal["gekiso"]["luckSamples"]),
            exchange,
        );
        let deadline = request["deadline"].as_f64().unwrap_or_else(|| {
            request["timeLimitMs"]
                .as_f64()
                .map(|ms| started + ms)
                .unwrap_or(f64::INFINITY)
        });
        let generation = crate::cancellation_generation();
        let shard = if request["shard"].is_null() {
            None
        } else {
            Some((
                master::int(&request["shard"]["index"]) as usize,
                master::int(&request["shard"]["count"]) as usize,
            ))
        };
        let mut surrogate = BonusSurrogate {
            members: cards
                .members
                .iter()
                .map(|m| power::member_event_percent(&route.effects, m, bonus_type) as f64)
                .collect(),
            snaps: cards
                .snaps
                .iter()
                .map(|s| power::snap_event_percent(&route.effects, s, bonus_type) as f64)
                .collect(),
            weight: 16777216.0,
        };
        let probe = search::search(search::Input {
            master: &self.master,
            player: &player,
            members: &cards.members,
            snaps: &cards.snaps,
            music: Some(&music),
            objective: &mut surrogate,
            constraints: constraints.clone(),
            k: 1,
            deadline: (now() + 100.0).min(deadline),
            now,
            seeds: Vec::new(),
            shard: None,
            owns: None,
            floor: f64::NEG_INFINITY,
            floor_exclusive: false,
            streaming: true,
            disable_snap_dominance: false,
            member_power_percent: None,
            snap_power_percent: None,
            on_candidate: None,
            progress: None,
        })?;
        let mut seeds = seed_teams(request, song, &cards.members, &cards.snaps);
        seeds.extend(probe.hits.into_iter().map(|h| h.team));
        let song_key = format!("{id}:{difficulty}");
        let floor = request["cutoff"]["key"].as_f64().unwrap_or_else(|| {
            master::number(&request["floors"][&song_key], f64::NEG_INFINITY)
                .max(master::number(&request["rewardCutoff"], f64::NEG_INFINITY))
        });
        let output = search::search(search::Input {
            master: &self.master,
            player: &player,
            members: &cards.members,
            snaps: &cards.snaps,
            music: Some(&music),
            objective: &mut objective,
            constraints,
            k: 1,
            deadline,
            now,
            seeds,
            shard,
            owns: None,
            floor,
            floor_exclusive: request["cutoff"]["comparison"] == "gt",
            streaming: true,
            disable_snap_dominance: true,
            member_power_percent: None,
            snap_power_percent: None,
            on_candidate: Some(&mut crate::progress::emit),
            progress: None,
        })?;
        let hits:Vec<_>=output.hits.iter().map(|hit|{let d=&hit.detail;let settlement=&d["settlement"];let count=master::number(&settlement["observations"],1.0);let cp=master::number(&settlement["cpSum"],0.0)/count;let mut hit=hit_json(hit,song,&cards.members,&cards.snaps,Some(chart),true);hit["gekiso"]=json!({"just":d["just"],"luckSamples":d["luckSamples"],"sampled":d["sampled"],"rank":1});hit["event"]=json!({"mean":master::number(&settlement["mean"],0.0)/exchange.map(|(_,d)|d).unwrap_or(1.0),"bonusPercent":master::number(&d["bonus"],0.0)/100.0,"challengePoints":cp,"perPlayMin":settlement["directMin"],"perPlayMax":settlement["directMax"],"rewardSum":settlement["rewardSum"],"comparisonSum":settlement["comparisonSum"],"orders":settlement["observations"],"directMean":master::number(&settlement["rewardSum"],0.0)/count,"convertedMean":exchange.map(|(n,d)|cp*n/d).unwrap_or(0.0)});hit}).collect();
        Ok(
            json!({"results":[{"song":song,"hits":hits,"proven":output.proven,"bound":output.bound,"stats":output.stats}],"overall":hits,"unknownCards":cards.unknown,"elapsedMs":now()-started,"stopReason":if generation!=crate::cancellation_generation(){Some("cancelled")}else if now()>=deadline{Some("budget")}else{None}}),
        )
    }
}
fn hit_json(
    hit: &crate::search::Hit,
    song: &Value,
    members: &[power::Member],
    snaps: &[power::Snap],
    chart: Option<&Chart>,
    battle: bool,
) -> Value {
    let mut value = json!({"song":song,"members":hit.team.members.iter().map(|&i|&members[i].key).collect::<Vec<_>>(),"snaps":hit.team.snaps.iter().map(|&j|if j<0{None}else{Some(&snaps[j as usize].key)}).collect::<Vec<_>>(),"power":hit.power,"slotPowers":hit.slot_powers,"key":hit.key});
    if let Some(chart) = chart.filter(|_| !hit.detail["scores"].is_null()) {
        let scores = &hit.detail["scores"];
        let values: Vec<_> = master::rows(&scores["scores"])
            .into_iter()
            .map(|v| master::number(v, 0.0))
            .collect();
        let mut sorted = values.clone();
        sorted.sort_by(f64::total_cmp);
        let mut ranks = BTreeMap::<i32, i32>::new();
        for &v in &values {
            let rank = if battle {
                chart::battle_rank(chart, v)
            } else {
                chart::score_rank(chart, v)
            };
            *ranks.entry(rank).or_default() += 1;
        }
        let keys = |order: usize| {
            live::orders()[order].map(|slot| members[hit.team.members[slot]].key.clone())
        };
        value["score"] = json!({"mean":scores["mean"],"min":scores["min"],"max":scores["max"],"median":(sorted[59]+sorted[60])/2.0,"orders":values.iter().map(|v|crate::num::round(*v)).collect::<Vec<_>>(),"bestOrder":keys(master::int(&scores["maxOrder"]) as usize),"worstOrder":keys(master::int(&scores["minOrder"]) as usize),"ranks":ranks});
    }
    value
}

impl Engine {
    fn load_source(&mut self, input: &Value, song: &Value) -> Result<(), String> {
        let key = (
            master::int(&song["songId"]),
            master::int(&song["difficulty"]),
        );
        if let Some(entry) = master::rows(&input["charts"])
            .into_iter()
            .find(|r| r["song"] == *song)
        {
            let source = if entry["source"].is_null() {
                crate::runtime_chart::convert(&entry["chart"])?
            } else {
                serde_json::from_value(entry["source"].clone()).map_err(|e| e.to_string())?
            };
            self.insert_chart(key.0, key.1, source)?;
        } else if self.charts.contains_key(&key) {
            self.chart_order.retain(|&old| old != key);
            self.chart_order.push_back(key);
        } else {
            return Err(format!("missing-chart:{}:{}", key.0, key.1));
        }
        Ok(())
    }
    pub fn plan(
        &mut self,
        input: &Value,
        started: f64,
        now: &dyn Fn() -> f64,
    ) -> Result<Value, String> {
        use crate::{
            event::{EventObjective, Route, RouteKind},
            objectives::{LiveCore, ScoreCap},
            search,
        };
        let request = &input["request"];
        let goal = &request["goal"];
        let deadline = request["deadline"].as_f64().unwrap_or_else(|| {
            request["timeLimitMs"]
                .as_f64()
                .map(|ms| started + ms)
                .unwrap_or(f64::INFINITY)
        });
        let generation = crate::cancellation_generation();
        let k = master::number(&request["k"], 1.0).max(1.0) as usize;
        let normal_songs = master::rows(&goal["normalSongs"]);
        let challenge_songs = master::rows(&goal["challengeSongs"]);
        let mut challenge = json!({"results":[],"overall":[],"unknownCards":[]});
        if goal["challengePointsPerPlay"].is_null() {
            let mut trial = input.clone();
            trial["request"]["goal"] = json!({"kind":"event","measure":"points","route":"challenge","eventId":goal["eventId"],"songs":challenge_songs,"consumption":goal["challengePointsPerLive"],"play":goal["play"]});
            if deadline.is_finite() {
                trial["request"]["deadline"] = json!(deadline);
            }
            challenge = self.run(&trial, started, now)?;
        }
        if master::rows(&challenge["results"])
            .into_iter()
            .any(|r| r["proven"] != true)
        {
            let mut results: Vec<_> = master::rows(&challenge["results"])
                .into_iter()
                .cloned()
                .collect();
            results.extend(normal_songs.iter().map(|song|json!({"song":song,"hits":[],"proven":false,"bound":null,"stats":search::Stats::default()})));
            return Ok(
                json!({"results":results,"overall":challenge["overall"],"unknownCards":challenge["unknownCards"],"elapsedMs":now()-started,"stopReason":if generation!=crate::cancellation_generation(){"cancelled"}else{"budget"},"cachedCharts":self.cached_chart_keys()}),
            );
        }
        let best_challenge = master::rows(&challenge["overall"])
            .first()
            .copied()
            .cloned()
            .unwrap_or(Value::Null);
        let points_per_challenge = master::number(
            &goal["challengePointsPerPlay"],
            master::number(&best_challenge["event"]["mean"], 0.0),
        );
        let cp_cost = master::int(&goal["challengePointsPerLive"]) as i64;
        let weight = if cp_cost > 0 {
            points_per_challenge / cp_cost as f64
        } else {
            0.0
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
        let route = Route::new(
            &self.master,
            master::int(&goal["eventId"]),
            RouteKind::Live,
            master::int(&goal["boostsPerLive"]),
        )?;
        crate::progress::keys(&cards.members, &cards.snaps);
        let mut normal = Vec::new();
        let mut progress_goal = goal.clone();
        progress_goal["planWeight"] = json!(weight);
        crate::progress::mode(&progress_goal);
        for (index, song) in normal_songs.iter().enumerate() {
            crate::progress::song(song);
            if now() >= deadline || generation != crate::cancellation_generation() {
                normal.extend(normal_songs[index..].iter().map(|song|json!({"song":song,"hits":[],"proven":false,"bound":null,"stats":search::Stats::default()})));
                break;
            }
            self.load_source(input, song)?;
            let id = master::int(&song["songId"]);
            let difficulty = master::int(&song["difficulty"]);
            let chart = self.charts.get(&(id, difficulty)).unwrap();
            let music = power::music_view(&self.master, id, None)?;
            crate::progress::chart(Some(chart), false);
            let budget = (deadline - now()).max(0.0) / (normal_songs.len() - index).max(1) as f64;
            let mut cap = ScoreCap {
                core: LiveCore::new(
                    &self.master,
                    chart,
                    &cards.members,
                    &cards.snaps,
                    &goal["play"],
                ),
            };
            let run = |objective: &mut dyn search::Objective, k, limit, notify| {
                let mut publish = crate::progress::emit;
                search::search(search::Input {
                    master: &self.master,
                    player: &player,
                    members: &cards.members,
                    snaps: &cards.snaps,
                    music: Some(&music),
                    objective,
                    constraints: constraints.clone(),
                    k,
                    deadline: limit,
                    now,
                    seeds: Vec::new(),
                    shard: None,
                    owns: None,
                    floor: f64::NEG_INFINITY,
                    floor_exclusive: false,
                    streaming: true,
                    disable_snap_dominance: false,
                    member_power_percent: None,
                    snap_power_percent: None,
                    on_candidate: if notify { Some(&mut publish) } else { None },
                    progress: None,
                })
            };
            let best = run(&mut cap, 1, (now() + budget / 4.0).min(deadline), false)?;
            let top = best.hits.first().map(|h| h.key).unwrap_or(f64::INFINITY);
            let score_cap = if best.proven {
                top
            } else {
                top.max(best.bound.unwrap_or(f64::INFINITY))
            };
            let mut objective = EventObjective::new(
                &self.master,
                &cards.members,
                &cards.snaps,
                Some(chart),
                route.clone(),
                true,
                &goal["play"],
                false,
                weight,
                None,
                score_cap,
            );
            let output = run(&mut objective, k, (now() + budget).min(deadline), true)?;
            let hits:Vec<_>=output.hits.iter().map(|h|{let mut value=hit_json(h,song,&cards.members,&cards.snaps,Some(chart),false);let d=&h.detail;let per_order:Vec<_>=master::rows(&d["perOrder"]).into_iter().map(|v|master::number(v,0.0)).collect();let points=if per_order.is_empty(){0.0}else{per_order.iter().sum::<f64>()/per_order.len() as f64};value["event"]=json!({"mean":points-weight*master::number(&d["challengePoints"],0.0),"bonusPercent":master::number(&d["bonus"],0.0)/100.0,"challengePoints":d["challengePoints"],"perPlayMin":0,"perPlayMax":0});value}).collect();
            normal.push(json!({"song":song,"hits":hits,"proven":output.proven,"bound":output.bound,"stats":output.stats}));
        }
        let best_normal = master::rows(&json!(normal))
            .into_iter()
            .flat_map(|r| master::rows(&r["hits"]))
            .cloned()
            .max_by(|a, b| compare_hits(b, a))
            .unwrap_or(Value::Null);
        let boosts = master::int(&goal["boostsPerLive"]) as i64;
        let budget = master::int(&goal["boostBudget"]) as i64;
        let lives = if boosts > 0 { budget / boosts } else { budget };
        let earned = (lives as f64 * master::number(&best_normal["event"]["challengePoints"], 0.0))
            .floor() as i64;
        let pool = master::int(&goal["startingChallengePoints"]) as i64 + earned;
        let challenge_lives = if cp_cost > 0 { pool / cp_cost } else { 0 };
        let points = lives as f64 * master::number(&best_normal["event"]["mean"], 0.0)
            + challenge_lives as f64 * points_per_challenge;
        let per_boost = if budget != 0 {
            points / (if boosts > 0 { lives * boosts } else { budget }).max(1) as f64
        } else {
            0.0
        };
        let plan = json!({"normal":best_normal,"challenge":best_challenge,"normalLives":lives,"challengeLives":challenge_lives,"challengePointsEarned":earned,"leftoverChallengePoints":pool-challenge_lives*cp_cost,"eventPoints":points,"perBoost":per_boost});
        let mut results: Vec<_> = master::rows(&challenge["results"])
            .into_iter()
            .cloned()
            .collect();
        results.extend(normal);
        let overall: Vec<_> = [best_normal, best_challenge]
            .into_iter()
            .filter(|v| !v.is_null())
            .collect();
        Ok(
            json!({"plan":plan,"results":results,"overall":overall,"unknownCards":if !goal["challengePointsPerPlay"].is_null(){json!(cards.unknown)}else{challenge["unknownCards"].clone()},"elapsedMs":now()-started,"cachedCharts":self.cached_chart_keys()}),
        )
    }
    fn cached_chart_keys(&self) -> Vec<String> {
        self.chart_order
            .iter()
            .map(|(s, d)| format!("{s}:{d}"))
            .collect()
    }
}
fn compare_hits(a: &Value, b: &Value) -> std::cmp::Ordering {
    master::number(&b["key"], 0.0)
        .total_cmp(&master::number(&a["key"], 0.0))
        .then_with(|| master::int(&a["song"]["songId"]).cmp(&master::int(&b["song"]["songId"])))
        .then_with(|| {
            master::int(&a["song"]["difficulty"]).cmp(&master::int(&b["song"]["difficulty"]))
        })
        .then_with(|| {
            master::number(&b["score"]["mean"], 0.0)
                .total_cmp(&master::number(&a["score"]["mean"], 0.0))
        })
}

impl Engine {
    pub fn evaluate_all(&mut self, input: &Value) -> Result<Value, String> {
        let request = &input["request"];
        let mut out = Vec::new();
        for song in master::rows(&request["songs"]) {
            self.load_source(input, song)?;
            out.push(self.evaluate(
                request,
                &request["team"],
                master::int(&song["songId"]),
                master::int(&song["difficulty"]),
            )?);
        }
        Ok(json!({"evaluations":out,"cachedCharts":self.cached_chart_keys()}))
    }
    pub fn timeline(&mut self, input: &Value) -> Result<Value, String> {
        let request = &input["request"];
        let song = &request["song"];
        self.load_source(input, song)?;
        let chart = self
            .charts
            .get(&(
                master::int(&song["songId"]),
                master::int(&song["difficulty"]),
            ))
            .unwrap();
        let members: Vec<MemberInput> =
            serde_json::from_value(request["members"].clone()).map_err(|e| e.to_string())?;
        let snaps: Vec<SnapInput> =
            serde_json::from_value(request["snaps"].clone()).map_err(|e| e.to_string())?;
        let box_ = power::resolve_box(
            &self.master,
            &members,
            &snaps,
            request["unknownPolicy"] != "min",
        );
        let members = master::rows(&request["memberKeys"])
            .into_iter()
            .map(|key| {
                box_.members
                    .iter()
                    .find(|m| key.as_str() == Some(&m.key))
                    .ok_or("unknown-team-member")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let snaps: Vec<_> = master::rows(&request["snapKeys"])
            .into_iter()
            .map(|key| box_.snaps.iter().find(|s| key.as_str() == Some(&s.key)))
            .collect();
        let duration = chart.last_note_ms as i64 + 1000;
        if duration < 0 {
            return Err("invalid-chart-duration".into());
        }
        let mut density = vec![0; ((duration + 499) / 500) as usize];
        for &time in &chart.times {
            let bin = time / 500;
            if bin < 0 || bin as usize >= density.len() {
                return Err("invalid-chart-note-time".into());
            }
            density[bin as usize] += 1;
        }
        let mut windows = Vec::new();
        for (event, slot) in master::integers(&request["order"]).into_iter().enumerate() {
            if event >= chart.skill_times.len() {
                continue;
            }
            let member = members.get(slot as usize).ok_or("invalid-skill-order")?;
            let snap = snaps.get(slot as usize).ok_or("invalid-snap-slot")?;
            let skill = skills::resolve(&self.master, member, *snap, &mut Vec::new());
            let (mut percent, mut perfect, mut length) = (0.0, 0.0, 0i32);
            for effect in &skill.effects {
                if effect.gate.blocks(self.master.live.life_base) {
                    continue;
                }
                if effect.kind == 2000 {
                    percent += effect.delta as f64 / 1000.0;
                } else {
                    perfect += effect.delta as f64 / 1000.0;
                }
                length = length.max(crate::num::window_ms(effect.seconds, skill.extension_ms));
            }
            let start = chart.skill_times[event];
            windows.push(json!({"member":member.key,"event":event,"startMs":start,"endMs":start as i64+length as i64,"percent":percent,"perfectPercent":perfect}));
        }
        Ok(
            json!({"durationMs":duration,"density":density,"fever":chart.fevers,"windows":windows,"cachedCharts":self.cached_chart_keys()}),
        )
    }
}

fn apply_reward_cutoff(response: &mut Value, request: &Value, song: &Value) {
    let cutoff = &request["cutoff"];
    let reward = request["rewardCutoff"].as_f64();
    if cutoff.is_null() && reward.is_none() {
        return;
    }
    let key = cutoff["key"].as_f64();
    let exclusive = cutoff["comparison"] == "gt";
    if let Some(results) = response["results"].as_array_mut() {
        for row in results {
            if let Some(hits) = row["hits"].as_array_mut() {
                hits.retain(|hit| {
                    let value = master::number(&hit["key"], 0.0);
                    key.is_none_or(|key| if exclusive { value > key } else { value >= key })
                        && reward.is_none_or(|key| value >= key)
                });
            }
            let empty = master::rows(&row["hits"]).is_empty();
            let bound = row["bound"].as_f64();
            if let Some(key) = key {
                if empty
                    && (row["proven"] == true
                        || bound.is_some_and(|b| if exclusive { b <= key } else { b < key }))
                {
                    row["exclusion"] = cutoff.clone();
                    row["proven"] = json!(false);
                    row["floor"] = Value::Null;
                }
            }
            if let Some(key) = reward {
                if empty && row["proven"] == true {
                    let song_key = format!(
                        "{}:{}",
                        master::int(&song["songId"]),
                        master::int(&song["difficulty"])
                    );
                    let excluded = key
                        .max(master::number(
                            &request["floors"][song_key],
                            f64::NEG_INFINITY,
                        ))
                        .max(master::number(&row["floor"], f64::NEG_INFINITY));
                    row["excludedBelow"] = json!(excluded);
                    row["proven"] = json!(false);
                    row["bound"] = json!(excluded);
                    row["floor"] = Value::Null;
                }
            }
        }
    }
    let mut hits: Vec<_> = master::rows(&response["results"])
        .into_iter()
        .flat_map(|r| master::rows(&r["hits"]))
        .cloned()
        .collect();
    hits.sort_by(compare_hits);
    hits.truncate(master::number(&request["k"], 1.0).max(1.0) as usize);
    response["overall"] = json!(hits);
}
