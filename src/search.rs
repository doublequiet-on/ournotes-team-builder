//! Exact top-K search over leaders, character-distinct member sets and snap
//! assignments. Bounds prune only when the retained legal teams justify it.
use crate::{
    assignment::Scratch,
    master::Master,
    power::{self, LeaderProfile, Member, Music, Player, SlotBase, Snap},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
#[derive(Clone, Copy, Default)]
pub struct Totals {
    pub power: f64,
    pub skill: f64,
    pub skill_low: f64,
    pub bonus: f64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Team {
    pub members: Vec<usize>,
    pub snaps: Vec<i32>,
}
pub struct Exact {
    pub key: f64,
    pub detail: Value,
}
pub enum Evaluation {
    Exact(Exact),
    Pruned(f64),
}
#[derive(Clone, Copy)]
pub struct Cutoff {
    pub key: f64,
    pub prune_equal: bool,
}
#[derive(Clone, Copy)]
pub struct JointTarget {
    pub bonus: f64,
    pub product: f64,
}
pub trait Objective {
    fn skill(&mut self, member: usize, snap: i32) -> (f64, f64);
    fn member_bonus(&self, _member: usize) -> f64 {
        0.0
    }
    fn snap_bonus(&self, _snap: usize) -> f64 {
        0.0
    }
    fn snap_dominance(&mut self, _member: usize, _snap: i32) -> Vec<f64> {
        Vec::new()
    }
    fn preserve_snap_options(&self) -> bool {
        false
    }
    fn bound(&mut self, totals: Totals) -> f64;
    fn member_bound(&mut self, _members: &[usize], _totals: Totals, _left: usize) -> Option<f64> {
        None
    }
    fn prepare_assignment(&mut self, _members: &[usize], _options: &[Vec<i32>]) {}
    fn assignment_bound(
        &mut self,
        _members: &[usize],
        _totals: Totals,
        _assigned: &[i32],
        _from: usize,
    ) -> Option<f64> {
        None
    }
    fn product_base(&self) -> Option<f64> {
        None
    }
    fn joint_targets(&mut self, _limit: f64) -> Option<Vec<JointTarget>> {
        None
    }
    /// A team bonus below this cannot reach `limit` (or the objective's own fixed target) at any score rank.
    fn bonus_floor(&mut self, _limit: f64) -> Option<f64> {
        None
    }
    fn prefer_snap(&self, _member: usize, _snap: i32) -> f64 {
        0.0
    }
    fn prefer_member(&self, _member: usize) -> f64 {
        0.0
    }
    fn leaf_bound(&mut self, _team: &Team, _totals: Totals) -> Option<f64> {
        None
    }
    fn interval(&mut self, team: &Team, totals: Totals) -> (f64, f64);
    fn exact(&mut self, team: &Team, power: f64, slot_powers: &[f64]) -> Result<Exact, String>;
    fn evaluate(
        &mut self,
        team: &Team,
        power: f64,
        slot_powers: &[f64],
        _cutoff: Cutoff,
    ) -> Result<Evaluation, String> {
        self.exact(team, power, slot_powers).map(Evaluation::Exact)
    }
}
#[derive(Default, Clone)]
pub struct Constraints {
    pub required_members: Vec<usize>,
    pub excluded_members: Vec<usize>,
    pub required_snaps: Vec<usize>,
    pub excluded_snaps: Vec<usize>,
    pub leader: Option<i32>,
    pub bindings: HashMap<usize, i32>,
    pub no_snaps: bool,
    pub min_bonus: Option<f64>,
    pub max_bonus: Option<f64>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub team: Team,
    pub power: f64,
    pub slot_powers: Vec<f64>,
    pub key: f64,
    pub detail: Value,
    #[serde(skip)]
    pub totals: Totals,
}
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub leaders: usize,
    pub member_nodes: usize,
    pub snap_nodes: usize,
    pub candidates: usize,
    pub exact: usize,
    pub elapsed_ms: f64,
}
impl Stats {
    pub fn add(&mut self, other: &Self) {
        self.leaders += other.leaders;
        self.member_nodes += other.member_nodes;
        self.snap_nodes += other.snap_nodes;
        self.candidates += other.candidates;
        self.exact += other.exact;
        self.elapsed_ms += other.elapsed_ms;
    }
}
#[derive(Serialize)]
pub struct Output {
    pub hits: Vec<Hit>,
    pub proven: bool,
    pub bound: Option<f64>,
    pub stats: Stats,
}
pub type Ownership<'a> = &'a dyn Fn(&[usize]) -> bool;
pub type CandidateListener<'a> = &'a mut dyn FnMut(&[Hit]);
pub struct Input<'a, O: Objective + ?Sized> {
    pub master: &'a Master,
    pub player: &'a Player,
    pub members: &'a [Member],
    pub snaps: &'a [Snap],
    pub music: Option<&'a Music>,
    pub objective: &'a mut O,
    pub constraints: Constraints,
    pub k: usize,
    pub deadline: f64,
    pub now: &'a dyn Fn() -> f64,
    pub seeds: Vec<Team>,
    pub shard: Option<(usize, usize)>,
    pub owns: Option<Ownership<'a>>,
    pub floor: f64,
    pub floor_exclusive: bool,
    pub streaming: bool,
    pub disable_snap_dominance: bool,
    pub member_power_percent: Option<&'a dyn Fn(&Member) -> i32>,
    pub snap_power_percent: Option<&'a dyn Fn(&Snap) -> i32>,
    pub on_candidate: Option<CandidateListener<'a>>,
    pub progress: Option<&'a mut dyn FnMut(usize, usize)>,
}
struct Frontier {
    k: usize,
    best: HashMap<String, f64>,
    sorted: Vec<f64>,
}
impl Frontier {
    fn threshold(&self) -> f64 {
        if self.sorted.len() >= self.k {
            self.sorted[self.k - 1]
        } else {
            f64::NEG_INFINITY
        }
    }
    fn offer(&mut self, key: &str, value: f64) {
        let previous = self.best.get(key).copied();
        if previous.is_some_and(|p| p >= value)
            || (previous.is_none() && self.sorted.len() >= self.k && value <= self.threshold())
        {
            return;
        }
        self.best.insert(key.to_owned(), value);
        if let Some(previous) = previous {
            if let Some(i) = self.sorted.iter().position(|&v| v == previous) {
                self.sorted.remove(i);
            }
        }
        let mut i = self.sorted.len();
        while i > 0 && self.sorted[i - 1] < value {
            i -= 1;
        }
        self.sorted.insert(i, value);
        if self.sorted.len() > self.k {
            let removed = self.sorted.pop().unwrap();
            if let Some(key) = self
                .best
                .iter()
                .find(|(_, v)| **v == removed)
                .map(|(k, _)| k.clone())
            {
                self.best.remove(&key);
            }
        }
    }
}
#[derive(Clone)]
struct Candidate {
    set_key: String,
    team: Team,
    power: f64,
    slot_powers: Vec<f64>,
    totals: Totals,
    low: f64,
    high: f64,
}
#[derive(Clone, Copy, Default)]
struct Best {
    power: f64,
    weight: f64,
    bonus: f64,
}
#[derive(Default)]
struct Tables {
    count: usize,
    columns: usize,
    column_snap: Vec<usize>,
    power: Vec<Vec<f64>>,
    weight: Vec<Vec<f64>>,
    bonus: Vec<Vec<f64>>,
    blended: Vec<Vec<f64>>,
    joint: Vec<Vec<f64>>,
}
impl Tables {
    fn new(from: usize, width: usize) -> Self {
        let rows = || vec![vec![0.0; width]; 5 - from];
        Self {
            count: 0,
            columns: 0,
            column_snap: vec![0; width],
            power: rows(),
            weight: rows(),
            bonus: rows(),
            blended: rows(),
            joint: rows(),
        }
    }
}
struct Assignment {
    team: Vec<usize>,
    set_key: String,
    base_power: Vec<f64>,
    options: Vec<Vec<i32>>,
    used: Vec<bool>,
    assigned: Vec<i32>,
    max_power: Vec<f64>,
    max_weight: Vec<f64>,
    max_bonus: Vec<f64>,
    set_low: f64,
}
struct Members {
    leader: usize,
    characters: Vec<Vec<usize>>,
    forced: usize,
    slot_power: Vec<f64>,
    char_power: Vec<Vec<f64>>,
    char_skill: Vec<Vec<f64>>,
    char_bonus: Vec<Vec<f64>>,
    char_member_bonus: Vec<Vec<f64>>,
    chosen: Vec<usize>,
}
const NONE: f64 = -1e15;
struct Runner<'a, O: Objective + ?Sized> {
    input: Input<'a, O>,
    started: f64,
    generation: u64,
    width: usize,
    bases: Vec<SlotBase>,
    snap_power: Vec<f64>,
    skill: Vec<f64>,
    skill_low: Vec<f64>,
    member_bonus: Vec<f64>,
    snap_bonus: Vec<f64>,
    any_snap_bonus: bool,
    bonus_order: Vec<usize>,
    snap_bonus_top5: f64,
    required: HashSet<usize>,
    required_snaps: HashSet<usize>,
    bound_elsewhere: HashSet<usize>,
    excluded_members: HashSet<usize>,
    excluded_snaps: HashSet<usize>,
    best: Vec<Best>,
    options: Vec<Vec<i32>>,
    /// The bonus floor the snap options were last restricted to.
    snap_floor: f64,
    profiles: HashMap<usize, Arc<LeaderProfile>>,
    frontier: Frontier,
    candidates: Vec<Candidate>,
    hits: Vec<Hit>,
    retained: HashMap<String, f64>,
    compact_at: usize,
    timed_out: bool,
    unexplored: f64,
    stats: Stats,
    pools: Vec<Tables>,
    matching: Scratch,
    column_of: Vec<usize>,
    column_stamp: Vec<u64>,
    stamp: u64,
}
fn suffix_tops(values: &[f64]) -> Vec<Vec<f64>> {
    let mut tops = vec![vec![0.0; 5]; values.len() + 1];
    let mut best = vec![f64::NEG_INFINITY; 4];
    for i in (0..values.len()).rev() {
        best.push(values[i]);
        best.sort_by(|a, b| b.total_cmp(a));
        best.truncate(4);
        for c in 1..=4 {
            tops[i][c] = tops[i][c - 1]
                + if best[c - 1] == f64::NEG_INFINITY {
                    0.0
                } else {
                    best[c - 1]
                };
        }
    }
    tops
}
fn top_sum(table: &[Vec<f64>], from: usize, count: usize) -> f64 {
    table[from.min(table.len() - 1)][count.min(4)]
}
fn suffix(values: &[f64], from: usize) -> f64 {
    values[from..].iter().rev().fold(0.0, |sum, v| sum + v)
}
impl<'a, O: Objective + ?Sized> Runner<'a, O> {
    fn new(mut input: Input<'a, O>) -> Self {
        let started = (input.now)();
        let generation = crate::cancellation_generation();
        input.k = input.k.max(1);
        let n = input.members.len();
        let s = input.snaps.len();
        let width = s + 1;
        let excluded_members = input.constraints.excluded_members.iter().copied().collect();
        let excluded_snaps: HashSet<_> = input.constraints.excluded_snaps.iter().copied().collect();
        let bases: Vec<_> = input
            .members
            .iter()
            .map(|m| {
                power::slot_base(
                    input.master,
                    input.player,
                    m,
                    input.music,
                    input.member_power_percent.map(|f| f(m)).unwrap_or(0),
                )
            })
            .collect();
        let snap_event: Vec<_> = input
            .snaps
            .iter()
            .map(|s| input.snap_power_percent.map(|f| f(s)).unwrap_or(0))
            .collect();
        let allowed: Vec<_> = (0..s)
            .filter(|j| !excluded_snaps.contains(j) && !input.constraints.no_snaps)
            .collect();
        let (mut snap_power, mut skill, mut skill_low) = (
            vec![0.0; n * width],
            vec![0.0; n * width],
            vec![0.0; n * width],
        );
        for i in 0..n {
            for &j in &allowed {
                snap_power[i * width + j] = power::snap_points(
                    input.master,
                    &bases[i],
                    &input.members[i],
                    Some(&input.snaps[j]),
                    snap_event[j],
                ) as f64;
                let (low, high) = input.objective.skill(i, j as i32);
                skill_low[i * width + j] = low;
                skill[i * width + j] = high;
            }
            let (low, high) = input.objective.skill(i, -1);
            skill_low[i * width + s] = low;
            skill[i * width + s] = high;
        }
        let member_bonus: Vec<_> = (0..n).map(|i| input.objective.member_bonus(i)).collect();
        let snap_bonus: Vec<_> = (0..s).map(|i| input.objective.snap_bonus(i)).collect();
        let any_snap_bonus = snap_bonus.iter().any(|&v| v != 0.0);
        let mut bonus_order: Vec<_> = allowed
            .iter()
            .copied()
            .filter(|&j| snap_bonus[j] > 0.0)
            .collect();
        bonus_order.sort_by(|&a, &b| snap_bonus[b].total_cmp(&snap_bonus[a]));
        let snap_bonus_top5 = bonus_order
            .iter()
            .take(5)
            .fold(0.0, |sum, &j| sum + snap_bonus[j]);
        let mut required: HashSet<_> = input.constraints.required_members.iter().copied().collect();
        required.extend(input.constraints.bindings.keys().copied());
        let mut required_snaps: HashSet<_> =
            input.constraints.required_snaps.iter().copied().collect();
        for &snap in input.constraints.bindings.values() {
            if snap >= 0 {
                required_snaps.remove(&(snap as usize));
            }
        }
        let bound_elsewhere: HashSet<_> = input
            .constraints
            .bindings
            .values()
            .filter(|&&j| j >= 0)
            .map(|&j| j as usize)
            .collect();
        let best: Vec<_> = (0..n)
            .map(|i| {
                let choices = input
                    .constraints
                    .bindings
                    .get(&i)
                    .map(|&b| if b < 0 || allowed.contains(&(b as usize)) { vec![b] } else { Vec::new() })
                    .unwrap_or_else(|| {
                        allowed
                            .iter()
                            .map(|&j| j as i32)
                            .chain(std::iter::once(-1))
                            .collect()
                    });
                let mut best = Best::default();
                for j in choices {
                    let col = if j < 0 { s } else { j as usize };
                    if col > s {
                        continue;
                    }
                    best.power = best.power.max(snap_power[i * width + col]);
                    best.weight = best.weight.max(skill[i * width + col]);
                    best.bonus = best
                        .bonus
                        .max(if j < 0 { 0.0 } else { snap_bonus[j as usize] });
                }
                best
            })
            .collect();
        let options = (0..n)
            .map(|i| {
                if let Some(&bound) = input.constraints.bindings.get(&i) {
                    return if bound < 0 || allowed.contains(&(bound as usize)) { vec![bound] } else { Vec::new() };
                }
                let list: Vec<i32> = allowed
                    .iter()
                    .copied()
                    .filter(|j| !bound_elsewhere.contains(j))
                    .map(|j| j as i32)
                    .chain(std::iter::once(-1))
                    .collect();
                let values: Vec<Vec<f64>> = list
                    .iter()
                    .map(|&j| {
                        let col = if j < 0 { s } else { j as usize };
                        let mut value = vec![
                            snap_power[i * width + col],
                            skill[i * width + col],
                            skill_low[i * width + col],
                            if j < 0 { 0.0 } else { snap_bonus[j as usize] },
                        ];
                        value.extend(input.objective.snap_dominance(i, j));
                        value
                    })
                    .collect();
                let mut kept: Vec<_> = list
                    .iter()
                    .enumerate()
                    .filter(|&(a, &j)| {
                        if input.disable_snap_dominance
                            || input.objective.preserve_snap_options()
                            || j < 0
                            || required_snaps.contains(&(j as usize))
                        {
                            return true;
                        }
                        let mut dominated = 0;
                        for (b, &other) in list.iter().enumerate() {
                            if b == a || other < 0 {
                                continue;
                            }
                            let (x, y) = (&values[a], &values[b]);
                            if y.iter().zip(x).all(|(y, x)| y >= x) {
                                let equal = y == x;
                                if !equal || b < a {
                                    dominated += 1;
                                    if dominated >= 5 {
                                        break;
                                    }
                                }
                            }
                        }
                        dominated < 5
                    })
                    .map(|(_, &j)| j)
                    .collect();
                kept.sort_by(|&a, &b| {
                    let preference =
                        input.objective.prefer_snap(i, b) - input.objective.prefer_snap(i, a);
                    if preference != 0.0 {
                        preference.total_cmp(&0.0)
                    } else {
                        snap_power[i * width + if b < 0 { s } else { b as usize }]
                            .total_cmp(&snap_power[i * width + if a < 0 { s } else { a as usize }])
                    }
                });
                kept
            })
            .collect();
        let k = input.k;
        Self {
            input,
            started,
            generation,
            width,
            bases,
            snap_power,
            skill,
            skill_low,
            member_bonus,
            snap_bonus,
            any_snap_bonus,
            bonus_order,
            snap_bonus_top5,
            required,
            required_snaps,
            bound_elsewhere,
            excluded_members,
            excluded_snaps,
            best,
            options,
            snap_floor: f64::NEG_INFINITY,
            profiles: HashMap::new(),
            frontier: Frontier {
                k,
                best: HashMap::new(),
                sorted: Vec::new(),
            },
            candidates: Vec::new(),
            hits: Vec::new(),
            retained: HashMap::new(),
            compact_at: 4096,
            timed_out: false,
            unexplored: f64::NEG_INFINITY,
            stats: Stats::default(),
            pools: (0..5).map(|from| Tables::new(from, s + 5)).collect(),
            matching: Scratch::default(),
            column_of: vec![0; s],
            column_stamp: vec![0; s],
            stamp: 0,
        }
    }
    fn expired(&self) -> bool {
        (self.input.now)() > self.input.deadline
            || self.generation != crate::cancellation_generation()
    }
    fn limit(&self) -> f64 {
        self.frontier.threshold().max(self.input.floor)
    }
    fn cannot_improve(&self, upper: f64) -> bool {
        (self.input.floor_exclusive && upper <= self.input.floor)
            || (self.input.streaming
                && self.hits.len() >= self.input.k
                && upper <= self.hits[self.input.k - 1].key)
    }
    fn set_key(&self, team: &[usize]) -> String {
        let mut keys: Vec<_> = team
            .iter()
            .map(|&i| self.input.members[i].key.as_str())
            .collect();
        keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        keys.join("|")
    }
    fn profile(&mut self, leader: usize) -> Arc<LeaderProfile> {
        self.profiles
            .entry(leader)
            .or_insert_with(|| {
                Arc::new(power::leader_profile(
                    self.input.master,
                    &self.input.members[leader],
                ))
            })
            .clone()
    }
    fn retain(&mut self, candidate: Candidate, evaluated: Option<Exact>) -> Result<(), String> {
        if !self.input.streaming {
            self.candidates.push(candidate);
            return Ok(());
        }
        if evaluated.is_none() && self.hits.len() >= self.input.k && self.expired() {
            self.timed_out = true;
            self.unexplored = self.unexplored.max(candidate.high);
            return Ok(());
        }
        if self.cannot_improve(candidate.high) {
            return Ok(());
        }
        let existing = self
            .hits
            .iter()
            .position(|h| self.set_key(&h.team.members) == candidate.set_key);
        if existing.is_some_and(|i| self.hits[i].key >= candidate.high) {
            return Ok(());
        }
        let local = (if self.hits.len() >= self.input.k {
            self.hits[self.input.k - 1].key
        } else {
            f64::NEG_INFINITY
        })
        .max(
            existing
                .map(|i| self.hits[i].key)
                .unwrap_or(f64::NEG_INFINITY),
        );
        let cutoff = Cutoff {
            key: local.max(self.input.floor),
            prune_equal: local >= self.input.floor && local.is_finite(),
        };
        let was_evaluated = evaluated.is_some();
        let exact = if let Some(exact) = evaluated {
            Evaluation::Exact(exact)
        } else {
            self.input.objective.evaluate(
                &candidate.team,
                candidate.power,
                &candidate.slot_powers,
                cutoff,
            )?
        };
        let exact = match exact {
            Evaluation::Exact(exact) => exact,
            Evaluation::Pruned(bound) => {
                if !(bound < cutoff.key || (cutoff.prune_equal && bound == cutoff.key)) {
                    return Err("unsafe-objective-cutoff".into());
                }
                return Ok(());
            }
        };
        if !was_evaluated {
            self.stats.exact += 1;
        }
        self.frontier.offer(&candidate.set_key, exact.key);
        if existing.is_some_and(|i| self.hits[i].key >= exact.key) {
            return Ok(());
        }
        if let Some(i) = existing {
            self.hits.remove(i);
        }
        let hit = Hit {
            team: candidate.team,
            power: candidate.power,
            slot_powers: candidate.slot_powers,
            totals: candidate.totals,
            key: exact.key,
            detail: exact.detail,
        };
        let mut i = self.hits.len();
        while i > 0 && self.hits[i - 1].key < hit.key {
            i -= 1;
        }
        self.hits.insert(i, hit);
        self.retained.insert(candidate.set_key, exact.key);
        if self.hits.len() > self.input.k {
            let hit = self.hits.pop().unwrap();
            let key = self.set_key(&hit.team.members);
            self.retained.remove(&key);
        }
        if let Some(callback) = self.input.on_candidate.as_deref_mut() {
            callback(&self.hits);
        }
        Ok(())
    }
    fn seed_valid(&self, seed: &Team) -> bool {
        let n = self.input.members.len();
        let s = self.input.snaps.len();
        let c = &self.input.constraints;
        if seed.members.len() != 5
            || seed.snaps.len() != 5
            || seed.members.iter().any(|&i| i >= n)
            || seed.snaps.iter().any(|&j| j < -1 || j >= s as i32)
        {
            return false;
        }
        if seed.members.iter().copied().collect::<HashSet<_>>().len() != 5
            || seed
                .members
                .iter()
                .any(|i| self.excluded_members.contains(i))
            || c.leader.is_some_and(|i| seed.members[0] as i32 != i)
            || seed
                .members
                .iter()
                .map(|&i| self.input.members[i].card.character_id)
                .collect::<HashSet<_>>()
                .len()
                != 5
            || self.required.iter().any(|i| !seed.members.contains(i))
        {
            return false;
        }
        let used: Vec<_> = seed
            .snaps
            .iter()
            .filter(|&&j| j >= 0)
            .map(|&j| j as usize)
            .collect();
        if used.iter().copied().collect::<HashSet<_>>().len() != used.len()
            || (c.no_snaps && !used.is_empty())
            || used.iter().any(|j| self.excluded_snaps.contains(j))
            || self.required_snaps.iter().any(|j| !used.contains(j))
        {
            return false;
        }
        for slot in 0..5 {
            let bound = c.bindings.get(&seed.members[slot]);
            if bound.is_some_and(|&b| b != seed.snaps[slot])
                || (bound.is_none()
                    && seed.snaps[slot] >= 0
                    && self.bound_elsewhere.contains(&(seed.snaps[slot] as usize)))
            {
                return false;
            }
        }
        true
    }
    fn evaluate_seed(&mut self, seed: Team) -> Result<(), String> {
        let profile = self.profile(seed.members[0]);
        let members: Vec<_> = seed
            .members
            .iter()
            .map(|&i| self.input.members[i].clone())
            .collect();
        let percentages =
            power::leader_percents(self.input.master, &profile, &members, 0, self.input.music);
        let powers: Vec<f64> = seed
            .members
            .iter()
            .enumerate()
            .map(|(slot, &i)| {
                self.bases[i].fixed_points as f64
                    + power::leader_points(&self.bases[i], &percentages[slot]) as f64
                    + self.snap_power[i * self.width
                        + if seed.snaps[slot] < 0 {
                            self.width - 1
                        } else {
                            seed.snaps[slot] as usize
                        }]
            })
            .collect();
        let power = powers.iter().fold(0.0, |a, b| a + b);
        let mut totals = Totals {
            power,
            ..Totals::default()
        };
        for (slot, &i) in seed.members.iter().enumerate() {
            let j = seed.snaps[slot];
            let col = if j < 0 { self.width - 1 } else { j as usize };
            totals.skill += self.skill[i * self.width + col];
            totals.skill_low += self.skill_low[i * self.width + col];
            totals.bonus += self.member_bonus[i]
                + if j < 0 {
                    0.0
                } else {
                    self.snap_bonus[j as usize]
                };
        }
        if self
            .input
            .constraints
            .min_bonus
            .is_some_and(|b| totals.bonus < b)
            || self
                .input
                .constraints
                .max_bonus
                .is_some_and(|b| totals.bonus > b)
        {
            return Ok(());
        }
        let exact = self.input.objective.exact(&seed, power, &powers)?;
        self.stats.exact += 1;
        let set_key = self.set_key(&seed.members);
        self.frontier.offer(&set_key, exact.key);
        self.retain(
            Candidate {
                set_key,
                team: seed,
                power,
                slot_powers: powers,
                totals,
                low: exact.key,
                high: exact.key,
            },
            Some(exact),
        )
    }
    fn member_set_bound(&mut self, members: &[usize], totals: Totals, left: usize) -> f64 {
        let plain = self.input.objective.bound(totals);
        if plain < self.limit() {
            return plain;
        }
        self.input
            .objective
            .member_bound(members, totals, left)
            .map(|bound| plain.min(bound))
            .unwrap_or(plain)
    }
    fn member_visit(
        &mut self,
        m: &mut Members,
        index: usize,
        power: f64,
        weight: f64,
        bonus: f64,
        member_part: f64,
    ) -> Result<(), String> {
        let left = 4 - m.chosen.len();
        if self.timed_out {
            let totals = Totals {
                power: power + top_sum(&m.char_power, index, left),
                skill: weight + top_sum(&m.char_skill, index, left),
                skill_low: 0.0,
                bonus: bonus + top_sum(&m.char_bonus, index, left),
            };
            let bound = self.input.objective.bound(totals);
            self.unexplored = self.unexplored.max(bound);
            return Ok(());
        }
        self.stats.member_nodes += 1;
        if left == 0 {
            let team: Vec<_> = std::iter::once(m.leader)
                .chain(m.chosen.iter().copied())
                .collect();
            if self.input.owns.is_none_or(|owns| owns(&team)) {
                self.assign(team)?;
            }
            return Ok(());
        }
        if m.characters.len() - index < left {
            return Ok(());
        }
        if index < m.forced {
            for i in m.characters[index].clone() {
                m.chosen.push(i);
                self.member_visit(
                    m,
                    index + 1,
                    power + m.slot_power[i] + self.best[i].power,
                    weight + self.best[i].weight,
                    bonus + self.member_bonus[i] + self.best[i].bonus,
                    member_part + self.member_bonus[i],
                )?;
                m.chosen.pop();
            }
            return Ok(());
        }
        let chosen: Vec<_> = std::iter::once(m.leader)
            .chain(m.chosen.iter().copied())
            .collect();
        let bound = self.member_set_bound(
            &chosen,
            Totals {
                power: power + top_sum(&m.char_power, index, left),
                skill: weight + top_sum(&m.char_skill, index, left),
                skill_low: 0.0,
                bonus: (bonus + top_sum(&m.char_bonus, index, left)).min(
                    member_part + top_sum(&m.char_member_bonus, index, left) + self.snap_bonus_top5,
                ),
            },
            left,
        );
        if bound < self.limit()
            || self.cannot_improve(bound)
            || self
                .input
                .constraints
                .min_bonus
                .is_some_and(|b| bonus + top_sum(&m.char_bonus, index, left) < b)
        {
            return Ok(());
        }
        let snap_bonus = self.best[m.leader].bonus
            + m.chosen
                .iter()
                .fold(0.0, |sum, &i| sum + self.best[i].bonus);
        if self
            .input
            .constraints
            .max_bonus
            .is_some_and(|b| bonus - snap_bonus > b)
        {
            return Ok(());
        }
        if self.expired() {
            self.timed_out = true;
            self.unexplored = self.unexplored.max(bound);
            return Ok(());
        }
        for i in m.characters[index].clone() {
            m.chosen.push(i);
            self.member_visit(
                m,
                index + 1,
                power + m.slot_power[i] + self.best[i].power,
                weight + self.best[i].weight,
                bonus + self.member_bonus[i] + self.best[i].bonus,
                member_part + self.member_bonus[i],
            )?;
            m.chosen.pop();
        }
        self.member_visit(m, index + 1, power, weight, bonus, member_part)
    }
    fn assignment_limit(&self, a: &Assignment) -> f64 {
        self.limit().max(a.set_low)
    }
    fn cannot_improve_set(&self, a: &Assignment, upper: f64) -> bool {
        self.cannot_improve(upper)
            || (self.input.streaming
                && self
                    .retained
                    .get(&a.set_key)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY)
                    >= upper)
    }
    fn assignment_bound(&mut self, a: &Assignment, totals: Totals, from: usize) -> f64 {
        self.input
            .objective
            .assignment_bound(&a.team, totals, &a.assigned, from)
            .unwrap_or_else(|| self.input.objective.bound(totals))
    }
    fn tables(&mut self, t: &mut Tables, a: &Assignment, from: usize) {
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.column_stamp.fill(0);
            self.stamp = 1;
        }
        let mut count = 0;
        for options in &a.options[from..] {
            for &j in options {
                if j >= 0 && !a.used[j as usize] && self.column_stamp[j as usize] != self.stamp {
                    self.column_stamp[j as usize] = self.stamp;
                    self.column_of[j as usize] = count;
                    t.column_snap[count] = j as usize;
                    count += 1;
                }
            }
        }
        t.count = count;
        t.columns = count + 5 - from;
        for row in 0..5 - from {
            let slot = from + row;
            let member = a.team[slot];
            t.power[row][..t.columns].fill(NONE);
            t.weight[row][..t.columns].fill(NONE);
            t.bonus[row][..t.columns].fill(NONE);
            for &j in &a.options[slot] {
                if j >= 0 && a.used[j as usize] {
                    continue;
                }
                let col = if j < 0 { self.width - 1 } else { j as usize };
                let power = self.snap_power[member * self.width + col];
                let weight = self.skill[member * self.width + col];
                let bonus = if j < 0 {
                    0.0
                } else {
                    self.snap_bonus[j as usize]
                };
                if j < 0 {
                    for empty in count..t.columns {
                        t.power[row][empty] = power;
                        t.weight[row][empty] = weight;
                        t.bonus[row][empty] = bonus;
                    }
                } else {
                    let at = self.column_of[j as usize];
                    t.power[row][at] = power;
                    t.weight[row][at] = weight;
                    t.bonus[row][at] = bonus;
                }
            }
        }
    }
    fn blend(t: &mut Tables, lambda: f64) {
        for r in 0..t.power.len() {
            for c in 0..t.columns {
                t.blended[r][c] = if t.power[r][c] <= NONE / 2.0 {
                    NONE
                } else {
                    lambda * t.power[r][c] + t.weight[r][c] / lambda
                };
            }
        }
    }
    fn joint_feasible(
        &mut self,
        t: &mut Tables,
        totals: Totals,
        room: Totals,
        base: f64,
        target: JointTarget,
    ) -> bool {
        let (power, skill, bonus) = (room.power, room.skill, room.bonus);
        let x = totals.power;
        let y = base + totals.skill;
        if totals.bonus + bonus < target.bonus || (x + power) * (y + skill) < target.product {
            return false;
        }
        if totals.bonus >= target.bonus || target.product <= 0.0 {
            return true;
        }
        let lambda = ((y + skill) / (x + power).max(1e-9)).sqrt();
        if !lambda.is_finite() || lambda <= 0.0 {
            return true;
        }
        let k = 2.0 * target.product.sqrt() - lambda * x - y / lambda;
        Self::blend(t, lambda);
        let spread = t
            .blended
            .iter()
            .flatten()
            .filter(|&&v| v > NONE / 2.0)
            .fold(0.0_f64, |best, &v| best.max(v));
        for factor in [0.25, 1.0, 4.0] {
            let mu = factor * spread / (target.bonus - totals.bonus).max(1.0);
            for r in 0..t.blended.len() {
                for c in 0..t.columns {
                    t.joint[r][c] = if t.blended[r][c] <= NONE / 2.0 {
                        NONE
                    } else {
                        t.blended[r][c] + mu * t.bonus[r][c]
                    };
                }
            }
            if self.matching.maximum(&t.joint, t.columns, None)
                < k + mu * (target.bonus - totals.bonus)
            {
                return false;
            }
        }
        true
    }
    fn remaining_bound(&mut self, a: &Assignment, from: usize, totals: Totals) -> f64 {
        let mut table = std::mem::take(&mut self.pools[from]);
        self.tables(&mut table, a, from);
        let power = self.matching.maximum(&table.power, table.columns, None);
        let skill = self.matching.maximum(&table.weight, table.columns, None);
        let bonus = if self.any_snap_bonus {
            self.matching.maximum(&table.bonus, table.columns, None)
        } else {
            0.0
        };
        let result = (|| {
            if power <= NONE / 2.0 || skill <= NONE / 2.0 {
                return f64::NEG_INFINITY;
            }
            let base = self.input.objective.product_base();
            if self.any_snap_bonus {
                if let Some(base) = base {
                    if let Some(targets) =
                        self.input.objective.joint_targets(self.assignment_limit(a))
                    {
                        if !targets.iter().any(|&target| {
                            self.joint_feasible(
                                &mut table,
                                totals,
                                Totals {
                                    power,
                                    skill,
                                    bonus,
                                    skill_low: 0.0,
                                },
                                base,
                                target,
                            )
                        }) {
                            return f64::NEG_INFINITY;
                        }
                    }
                }
            }
            let Some(base) = base else {
                return self.assignment_bound(
                    a,
                    Totals {
                        power: totals.power + power,
                        skill: totals.skill + skill,
                        skill_low: 0.0,
                        bonus: totals.bonus + bonus,
                    },
                    from,
                );
            };
            let x = totals.power;
            let y = base + totals.skill;
            if y + skill <= 0.0
                || x + power <= 0.0
                || !(y + skill).is_finite()
                || !(x + power).is_finite()
            {
                return self.assignment_bound(
                    a,
                    Totals {
                        power: x + power,
                        skill: totals.skill + skill,
                        skill_low: 0.0,
                        bonus: totals.bonus + bonus,
                    },
                    from,
                );
            }
            let mut product = (x + power) * (y + skill);
            if self.assignment_bound(
                a,
                Totals {
                    power: product / (y + skill),
                    skill: totals.skill + skill,
                    skill_low: 0.0,
                    bonus: totals.bonus + bonus,
                },
                from,
            ) < self.assignment_limit(a)
            {
                return f64::NEG_INFINITY;
            }
            let lambda = ((y + skill) / (x + power).max(1e-9)).sqrt();
            for scale in [1.0, 1.02, 1.0 / 1.02] {
                let l = lambda * scale;
                Self::blend(&mut table, l);
                let m = self.matching.maximum(&table.blended, table.columns, None);
                product = product.min((l * x + y / l + m).powi(2) / 4.0);
            }
            self.assignment_bound(
                a,
                Totals {
                    power: product / (y + skill),
                    skill: totals.skill + skill,
                    skill_low: 0.0,
                    bonus: totals.bonus + bonus,
                },
                from,
            )
        })();
        self.pools[from] = table;
        result
    }
    fn candidate(
        &mut self,
        a: &mut Assignment,
        picks: Vec<i32>,
        totals: Totals,
    ) -> Result<(), String> {
        let team = Team {
            members: a.team.clone(),
            snaps: picks,
        };
        if self
            .input
            .objective
            .leaf_bound(&team, totals)
            .is_some_and(|v| v < self.assignment_limit(a))
        {
            return Ok(());
        }
        let (low, high) = self.input.objective.interval(&team, totals);
        if high < self.assignment_limit(a) || self.cannot_improve_set(a, high) {
            return Ok(());
        }
        self.frontier.offer(&a.set_key, low);
        a.set_low = a.set_low.max(low);
        self.stats.candidates += 1;
        if self.candidates.len() > self.compact_at {
            self.compact();
        }
        let powers = team
            .members
            .iter()
            .enumerate()
            .map(|(slot, &i)| {
                a.base_power[slot]
                    + self.snap_power[i * self.width
                        + if team.snaps[slot] < 0 {
                            self.width - 1
                        } else {
                            team.snaps[slot] as usize
                        }]
            })
            .collect();
        self.retain(
            Candidate {
                set_key: a.set_key.clone(),
                team,
                power: totals.power,
                slot_powers: powers,
                totals,
                low,
                high,
            },
            None,
        )
    }
    fn assign(&mut self, team: Vec<usize>) -> Result<(), String> {
        let set_key = self.set_key(&team);
        let profile = self.profile(team[0]);
        let percents = if profile.simple {
            None
        } else {
            let members: Vec<_> = team
                .iter()
                .map(|&i| self.input.members[i].clone())
                .collect();
            Some(power::leader_percents(
                self.input.master,
                &profile,
                &members,
                0,
                self.input.music,
            ))
        };
        let base_power: Vec<_> = team
            .iter()
            .enumerate()
            .map(|(slot, &i)| {
                self.bases[i].fixed_points as f64
                    + power::leader_points(
                        &self.bases[i],
                        &percents.as_ref().map(|p| p[slot]).unwrap_or_else(|| {
                            power::leader_percent_bound(
                                self.input.master,
                                &profile,
                                &self.input.members[i],
                            )
                        }),
                    ) as f64
            })
            .collect();
        let options: Vec<_> = team.iter().map(|&i| self.options[i].clone()).collect();
        self.input.objective.prepare_assignment(&team, &options);
        let mut used = vec![false; self.input.snaps.len()];
        for (&member, &snap) in &self.input.constraints.bindings {
            if snap >= 0 && !team.contains(&member) {
                used[snap as usize] = true;
            }
        }
        let max_power = team.iter().map(|&i| self.best[i].power).collect();
        let max_weight = team.iter().map(|&i| self.best[i].weight).collect();
        let max_bonus = team.iter().map(|&i| self.best[i].bonus).collect();
        let member_bonus = team.iter().fold(0.0, |sum, &i| sum + self.member_bonus[i]);
        let mut a = Assignment {
            team,
            set_key,
            base_power,
            options,
            used,
            assigned: vec![-1; 5],
            max_power,
            max_weight,
            max_bonus,
            set_low: f64::NEG_INFINITY,
        };
        let base_total = a.base_power.iter().fold(0.0, |sum, v| sum + v);
        if self.assignment_bound(
            &a,
            Totals {
                power: base_total + suffix(&a.max_power, 0),
                skill: suffix(&a.max_weight, 0),
                skill_low: 0.0,
                bonus: member_bonus + suffix(&a.max_bonus, 0),
            },
            0,
        ) < self.assignment_limit(&a)
            || self
                .input
                .constraints
                .max_bonus
                .is_some_and(|b| member_bonus > b)
        {
            return Ok(());
        }
        if let Some(minimum) = self.input.constraints.min_bonus {
            let bonus = if self.any_snap_bonus {
                let mut table = std::mem::take(&mut self.pools[0]);
                self.tables(&mut table, &a, 0);
                let bonus = self.matching.maximum(&table.bonus, table.columns, None);
                self.pools[0] = table;
                bonus
            } else {
                0.0
            };
            if member_bonus + bonus < minimum {
                return Ok(());
            }
        }
        let upper = self.remaining_bound(
            &a,
            0,
            Totals {
                power: base_total,
                bonus: member_bonus,
                ..Totals::default()
            },
        );
        if upper < self.assignment_limit(&a) || self.cannot_improve_set(&a, upper) {
            return Ok(());
        }
        if let Some(base) = self.input.objective.product_base() {
            if self.required_snaps.is_empty() {
                let mut t = std::mem::take(&mut self.pools[0]);
                self.tables(&mut t, &a, 0);
                let p = self.matching.maximum(&t.power, t.columns, None);
                let s = self.matching.maximum(&t.weight, t.columns, None);
                let lambda = ((base + s) / (base_total + p).max(1e-9)).sqrt();
                let mut columns = vec![0; 5];
                let incumbent = if lambda.is_finite() && lambda > 0.0 {
                    Self::blend(&mut t, lambda);
                    self.matching
                        .maximum(&t.blended, t.columns, Some(&mut columns))
                } else {
                    f64::NEG_INFINITY
                };
                let picks: Vec<_> = columns
                    .iter()
                    .map(|&c| {
                        if c < t.count {
                            t.column_snap[c] as i32
                        } else {
                            -1
                        }
                    })
                    .collect();
                self.pools[0] = t;
                if incumbent > NONE / 2.0 {
                    let mut totals = Totals {
                        power: base_total,
                        bonus: member_bonus,
                        ..Totals::default()
                    };
                    for (slot, &i) in a.team.iter().enumerate() {
                        let j = picks[slot];
                        let col = if j < 0 { self.width - 1 } else { j as usize };
                        totals.power += self.snap_power[i * self.width + col];
                        totals.skill += self.skill[i * self.width + col];
                        totals.skill_low += self.skill_low[i * self.width + col];
                        totals.bonus += if j < 0 {
                            0.0
                        } else {
                            self.snap_bonus[j as usize]
                        };
                    }
                    if self
                        .input
                        .constraints
                        .min_bonus
                        .is_none_or(|b| totals.bonus >= b)
                        && self
                            .input
                            .constraints
                            .max_bonus
                            .is_none_or(|b| totals.bonus <= b)
                    {
                        self.candidate(&mut a, picks, totals)?;
                    }
                }
            }
        }
        self.snap_walk(
            &mut a,
            0,
            Totals {
                power: base_total,
                bonus: member_bonus,
                ..Totals::default()
            },
        )
    }
    fn snap_walk(&mut self, a: &mut Assignment, slot: usize, totals: Totals) -> Result<(), String> {
        if !self.timed_out {
            self.stats.snap_nodes += 1;
            if self.stats.snap_nodes & 1023 == 0 && self.expired() {
                self.timed_out = true;
            }
        }
        if self.timed_out {
            let bound = self.assignment_bound(
                a,
                Totals {
                    power: totals.power + suffix(&a.max_power, slot),
                    skill: totals.skill + suffix(&a.max_weight, slot),
                    skill_low: 0.0,
                    bonus: totals.bonus + suffix(&a.max_bonus, slot),
                },
                0,
            );
            self.unexplored = self.unexplored.max(bound);
            return Ok(());
        }
        let required_left = self.required_snaps.iter().filter(|&&j| !a.used[j]).count();
        if slot == 5 {
            if required_left > 0
                || self
                    .input
                    .constraints
                    .min_bonus
                    .is_some_and(|b| totals.bonus < b)
                || self
                    .input
                    .constraints
                    .max_bonus
                    .is_some_and(|b| totals.bonus > b)
                || self.assignment_bound(a, totals, 5) < self.assignment_limit(a)
            {
                return Ok(());
            }
            return self.candidate(a, a.assigned.clone(), totals);
        }
        let mut count = 5 - slot;
        let mut unused_bonus = 0.0;
        for &j in &self.bonus_order {
            if count == 0 {
                break;
            }
            if a.used[j] {
                continue;
            }
            unused_bonus += self.snap_bonus[j];
            count -= 1;
        }
        let optimistic = self.assignment_bound(
            a,
            Totals {
                power: totals.power + suffix(&a.max_power, slot),
                skill: totals.skill + suffix(&a.max_weight, slot),
                skill_low: 0.0,
                bonus: totals.bonus + suffix(&a.max_bonus, slot).min(unused_bonus),
            },
            slot,
        );
        if optimistic < self.assignment_limit(a)
            || self.cannot_improve_set(a, optimistic)
            || (slot > 0
                && slot < 4
                && self.remaining_bound(a, slot, totals) < self.assignment_limit(a))
            || 5 - slot < required_left
        {
            return Ok(());
        }
        let floor = if self.any_snap_bonus {
            let limit = self.assignment_limit(a);
            self.input.objective.bonus_floor(limit)
        } else {
            None
        };
        let mut rest = 0.0;
        if floor.is_some() {
            let mut count = 4 - slot;
            for &j in &self.bonus_order {
                if count == 0 {
                    break;
                }
                if !a.used[j] {
                    rest += self.snap_bonus[j];
                    count -= 1;
                }
            }
        }
        let member = a.team[slot];
        let mut options = a.options[slot].clone();
        if slot == 4 && self.input.objective.product_base().is_some() {
            let bar = self.assignment_limit(a);
            options.retain(|&j| {
                if j >= 0 && a.used[j as usize] {
                    return false;
                }
                let col = if j < 0 { self.width - 1 } else { j as usize };
                self.assignment_bound(
                    a,
                    Totals {
                        power: totals.power + self.snap_power[member * self.width + col],
                        skill: totals.skill + self.skill[member * self.width + col],
                        skill_low: 0.0,
                        bonus: totals.bonus
                            + if j < 0 {
                                0.0
                            } else {
                                self.snap_bonus[j as usize]
                            },
                    },
                    0,
                ) >= bar
            });
        }
        for j in options {
            if j >= 0 && a.used[j as usize] {
                continue;
            }
            if let Some(floor) = floor {
                let own = if j < 0 {
                    0.0
                } else {
                    self.snap_bonus[j as usize]
                };
                if totals.bonus + own + rest < floor - 1e-6 {
                    continue;
                }
            }
            let col = if j < 0 { self.width - 1 } else { j as usize };
            if j >= 0 {
                a.used[j as usize] = true;
            }
            a.assigned[slot] = j;
            self.snap_walk(
                a,
                slot + 1,
                Totals {
                    power: totals.power + self.snap_power[member * self.width + col],
                    skill: totals.skill + self.skill[member * self.width + col],
                    skill_low: totals.skill_low + self.skill_low[member * self.width + col],
                    bonus: totals.bonus
                        + if j < 0 {
                            0.0
                        } else {
                            self.snap_bonus[j as usize]
                        },
                },
            )?;
            if j >= 0 {
                a.used[j as usize] = false;
            }
        }
        a.assigned[slot] = -1;
        Ok(())
    }
    fn compact(&mut self) {
        let threshold = self.limit();
        let mut lows: HashMap<String, f64> = HashMap::new();
        for c in &self.candidates {
            lows.entry(c.set_key.clone())
                .and_modify(|v| *v = v.max(c.low))
                .or_insert(c.low);
        }
        self.candidates
            .retain(|c| c.high >= threshold && c.high >= lows[&c.set_key]);
        self.compact_at = 4096.max(self.candidates.len() * 2);
    }
    /// Drops every snap, and the empty slot, that cannot be part of a team whose bonus reaches `floor`, even with
    /// the best members and the best other snaps, and tightens each member's best snap to what remains.
    fn restrict_snaps(&mut self, floor: f64) {
        if floor <= self.snap_floor + 1e-6 || floor.is_nan() {
            return;
        }
        self.snap_floor = floor;
        let mut members: Vec<f64> = (0..self.member_bonus.len())
            .filter(|i| !self.excluded_members.contains(i))
            .map(|i| self.member_bonus[i])
            .collect();
        members.sort_by(|a, b| b.total_cmp(a));
        let member_top: f64 = members.iter().take(5).sum();
        let order = self.bonus_order.clone();
        let snap_bonus = &self.snap_bonus;
        let top_without = |skip: Option<usize>, count: usize| -> f64 {
            order
                .iter()
                .filter(|&&j| Some(j) != skip)
                .take(count)
                .map(|&j| snap_bonus[j])
                .sum()
        };
        let empty_ok = top_without(None, 4) + member_top >= floor - 1e-6;
        let fits = |j: i32| {
            if j < 0 {
                empty_ok
            } else {
                let j = j as usize;
                self.required_snaps.contains(&j)
                    || snap_bonus[j] + top_without(Some(j), 4) + member_top >= floor - 1e-6
            }
        };
        let width = self.width;
        for i in 0..self.options.len() {
            if self.input.constraints.bindings.contains_key(&i) {
                continue;
            }
            let kept: Vec<i32> = self.options[i].iter().copied().filter(|&j| fits(j)).collect();
            if kept.len() == self.options[i].len() {
                continue;
            }
            let mut best = Best::default();
            for &j in &kept {
                let col = if j < 0 { width - 1 } else { j as usize };
                best.power = best.power.max(self.snap_power[i * width + col]);
                best.weight = best.weight.max(self.skill[i * width + col]);
                best.bonus = best.bonus.max(if j < 0 { 0.0 } else { snap_bonus[j as usize] });
            }
            if kept.is_empty() {
                best = Best { power: NONE, weight: NONE, bonus: NONE };
            }
            self.best[i] = best;
            self.options[i] = kept;
        }
        self.bonus_order.retain(|&j| fits(j as i32));
        self.snap_bonus_top5 = self.bonus_order.iter().take(5).map(|&j| self.snap_bonus[j]).sum();
    }
    fn run(mut self) -> Result<Output, String> {
        let n = self.input.members.len();
        let mut leaders: Vec<_> = (0..n)
            .filter(|i| {
                !self.excluded_members.contains(i)
                    && self.input.constraints.leader.is_none_or(|l| l == *i as i32)
            })
            .collect();
        leaders.sort_by(|&a, &b| {
            let hint =
                self.input.objective.prefer_member(b) - self.input.objective.prefer_member(a);
            if hint != 0.0 {
                hint.total_cmp(&0.0)
            } else {
                (self.bases[b].fixed_points as f64 + self.best[b].power)
                    .total_cmp(&(self.bases[a].fixed_points as f64 + self.best[a].power))
            }
        });
        if let Some((index, count)) = self.input.shard {
            if count > 1 {
                leaders = leaders
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| i % count == index)
                    .map(|(_, leader)| leader)
                    .collect();
            }
        }
        if leaders.is_empty() {
            return Ok(Output {
                hits: Vec::new(),
                proven: true,
                bound: None,
                stats: self.stats,
            });
        }
        let mut seen = HashSet::new();
        for seed in self.input.seeds.clone() {
            if self.expired() {
                break;
            }
            if self.seed_valid(&seed) {
                let key = format!("{:?}|{:?}", seed.members, seed.snaps);
                if seen.insert(key) {
                    self.evaluate_seed(seed)?;
                }
            }
        }
        for (position, &leader) in leaders.iter().enumerate() {
            if let Some(callback) = self.input.progress.as_deref_mut() {
                callback(position, leaders.len());
            }
            if self.expired() {
                self.timed_out = true;
            }
            self.stats.leaders += 1;
            if self.any_snap_bonus {
                let limit = self.limit();
                if let Some(floor) = self.input.objective.bonus_floor(limit) {
                    self.restrict_snaps(floor);
                }
            }
            let profile = self.profile(leader);
            let character = self.input.members[leader].card.character_id;
            if self.required.iter().any(|&i| {
                i != leader && (i >= n || self.input.members[i].card.character_id == character)
            }) {
                continue;
            }
            let slot_power: Vec<_> = (0..n)
                .map(|i| {
                    self.bases[i].fixed_points as f64
                        + power::leader_points(
                            &self.bases[i],
                            &power::leader_percent_bound(
                                self.input.master,
                                &profile,
                                &self.input.members[i],
                            ),
                        ) as f64
                })
                .collect();
            let mut groups: Vec<(i32, Vec<usize>)> = Vec::new();
            for i in 0..n {
                if i == leader
                    || self.excluded_members.contains(&i)
                    || self.input.members[i].card.character_id == character
                {
                    continue;
                }
                let ch = self.input.members[i].card.character_id;
                if let Some((_, list)) = groups.iter_mut().find(|(c, _)| *c == ch) {
                    list.push(i);
                } else {
                    groups.push((ch, vec![i]));
                }
            }
            let (mut forced, mut free) = (Vec::new(), Vec::new());
            let mut valid = true;
            for (_, list) in groups {
                let must: Vec<_> = list
                    .iter()
                    .copied()
                    .filter(|i| self.required.contains(i))
                    .collect();
                if must.len() > 1 {
                    valid = false;
                    break;
                }
                let required = !must.is_empty();
                let mut list = if required { must } else { list };
                list.sort_by(|&a, &b| {
                    let hint = self.input.objective.prefer_member(b)
                        - self.input.objective.prefer_member(a);
                    if hint != 0.0 {
                        hint.total_cmp(&0.0)
                    } else {
                        (slot_power[b] + self.best[b].power)
                            .total_cmp(&(slot_power[a] + self.best[a].power))
                    }
                });
                if required {
                    forced.push(list);
                } else {
                    free.push(list);
                }
            }
            if !valid || forced.len() > 4 {
                continue;
            }
            free.sort_by(|a, b| {
                let hint = self.input.objective.prefer_member(b[0])
                    - self.input.objective.prefer_member(a[0]);
                if hint != 0.0 {
                    hint.total_cmp(&0.0)
                } else {
                    (slot_power[b[0]] + self.best[b[0]].power)
                        .total_cmp(&(slot_power[a[0]] + self.best[a[0]].power))
                }
            });
            let forced_count = forced.len();
            let characters: Vec<_> = forced.into_iter().chain(free).collect();
            if characters.len() < 4 {
                continue;
            }
            let char_power = suffix_tops(
                &characters
                    .iter()
                    .map(|list| {
                        list.iter()
                            .map(|&i| slot_power[i] + self.best[i].power)
                            .fold(f64::NEG_INFINITY, f64::max)
                    })
                    .collect::<Vec<_>>(),
            );
            let char_skill = suffix_tops(
                &characters
                    .iter()
                    .map(|list| {
                        list.iter()
                            .map(|&i| self.best[i].weight)
                            .fold(f64::NEG_INFINITY, f64::max)
                    })
                    .collect::<Vec<_>>(),
            );
            let char_bonus = suffix_tops(
                &characters
                    .iter()
                    .map(|list| {
                        list.iter()
                            .map(|&i| self.member_bonus[i] + self.best[i].bonus)
                            .fold(f64::NEG_INFINITY, f64::max)
                    })
                    .collect::<Vec<_>>(),
            );
            let char_member_bonus = suffix_tops(
                &characters
                    .iter()
                    .map(|list| {
                        list.iter()
                            .map(|&i| self.member_bonus[i])
                            .fold(f64::NEG_INFINITY, f64::max)
                    })
                    .collect::<Vec<_>>(),
            );
            let leader_totals = Totals {
                power: slot_power[leader] + self.best[leader].power,
                skill: self.best[leader].weight,
                skill_low: 0.0,
                bonus: self.member_bonus[leader] + self.best[leader].bonus,
            };
            let bound = self.member_set_bound(
                &[leader],
                Totals {
                    power: leader_totals.power + top_sum(&char_power, 0, 4),
                    skill: leader_totals.skill + top_sum(&char_skill, 0, 4),
                    skill_low: 0.0,
                    bonus: (leader_totals.bonus + top_sum(&char_bonus, 0, 4)).min(
                        self.member_bonus[leader]
                            + top_sum(&char_member_bonus, 0, 4)
                            + self.snap_bonus_top5,
                    ),
                },
                4,
            );
            if bound < self.limit() || self.cannot_improve(bound) {
                continue;
            }
            if self.timed_out {
                self.unexplored = self.unexplored.max(bound);
                continue;
            }
            if self
                .input
                .constraints
                .min_bonus
                .is_some_and(|b| leader_totals.bonus + top_sum(&char_bonus, 0, 4) < b)
            {
                continue;
            }
            let mut members = Members {
                leader,
                characters,
                forced: forced_count,
                slot_power,
                char_power,
                char_skill,
                char_bonus,
                char_member_bonus,
                chosen: Vec::with_capacity(4),
            };
            self.member_visit(
                &mut members,
                0,
                leader_totals.power,
                leader_totals.skill,
                leader_totals.bonus,
                self.member_bonus[leader],
            )?;
        }
        if !self.input.streaming {
            let threshold = self.limit();
            let mut pool = std::mem::take(&mut self.candidates);
            pool.retain(|c| c.high >= threshold);
            pool.sort_by(|a, b| b.high.total_cmp(&a.high));
            let mut known: HashMap<String, f64> = HashMap::new();
            for c in pool {
                let kth = if self.hits.len() < self.input.k {
                    f64::NEG_INFINITY
                } else {
                    self.hits[self.input.k - 1].key
                };
                if c.high < kth {
                    break;
                }
                if self.expired() && self.hits.len() >= self.input.k {
                    self.timed_out = true;
                    self.unexplored = self.unexplored.max(c.high);
                    break;
                }
                if known.get(&c.set_key).is_some_and(|&v| v >= c.high) {
                    continue;
                }
                self.stats.exact += 1;
                let exact = self
                    .input
                    .objective
                    .exact(&c.team, c.power, &c.slot_powers)?;
                if known.get(&c.set_key).is_some_and(|&v| v >= exact.key) {
                    continue;
                }
                known.insert(c.set_key.clone(), exact.key);
                if let Some(i) = self
                    .hits
                    .iter()
                    .position(|h| self.set_key(&h.team.members) == c.set_key)
                {
                    self.hits.remove(i);
                }
                let hit = Hit {
                    team: c.team,
                    power: c.power,
                    slot_powers: c.slot_powers,
                    totals: c.totals,
                    key: exact.key,
                    detail: exact.detail,
                };
                let mut i = self.hits.len();
                while i > 0 && self.hits[i - 1].key < hit.key {
                    i -= 1;
                }
                self.hits.insert(i, hit);
            }
            self.hits.truncate(self.input.k);
        }
        self.stats.elapsed_ms = (self.input.now)() - self.started;
        Ok(Output {
            hits: self.hits,
            proven: !self.timed_out,
            bound: if self.timed_out {
                Some(self.unexplored)
            } else {
                None
            },
            stats: self.stats,
        })
    }
}
pub fn search<O: Objective + ?Sized>(input: Input<'_, O>) -> Result<Output, String> {
    Runner::new(input).run()
}
