//! Ordinary score and songless-potential objective adapters.
use crate::{
    chart::{self, Chart},
    live::{self, Input, Scores, Slot},
    master::Master,
    model::{self, Linear, PlayBounds, Weights, RELATIVE_ERROR},
    num::window_ms,
    power::{Member, Snap},
    search::{Exact, Objective, Team, Totals},
    skills,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
#[derive(Clone, Copy, PartialEq)]
pub enum Criterion {
    Mean,
    Min,
    Max,
}
impl Criterion {
    pub fn from_value(v: &Value) -> Self {
        match v.as_str() {
            Some("min") => Self::Min,
            Some("max") => Self::Max,
            _ => Self::Mean,
        }
    }
    pub fn key(self, scores: &Scores) -> f64 {
        match self {
            Self::Mean => scores.mean,
            Self::Min => scores.min,
            Self::Max => scores.max,
        }
    }
}
#[derive(Clone)]
pub(crate) struct Pair {
    pub high: Weights,
    pub low: Weights,
}
pub struct LiveCore {
    pub input: Input,
    pub high: Linear,
    pub low: Linear,
    pub factor_cap: f64,
    pub max_recovery: i32,
    pub drains: bool,
    pub slots: Vec<Slot>,
    ids: Vec<usize>,
    width: usize,
    weights: HashMap<usize, Pair>,
    team_models: HashMap<i32, (Linear, HashMap<usize, f64>)>,
    play_bounds: Option<PlayBounds>,
    scores: HashMap<(i32, [usize; 5]), Arc<Scores>>,
    score_order: VecDeque<(i32, [usize; 5])>,
}
impl LiveCore {
    pub fn new(
        master: &Master,
        chart: &Chart,
        members: &[Member],
        snaps: &[Snap],
        play: &Value,
    ) -> Self {
        let width = snaps.len() + 1;
        let mut slots = Vec::with_capacity(members.len() * width);
        let mut ids = Vec::with_capacity(members.len() * width);
        let mut intern: HashMap<String, usize> = HashMap::new();
        let mut unsupported = Vec::new();
        for member in members {
            for snap in std::iter::once(None).chain(snaps.iter().map(Some)) {
                let slot = skills::resolve(master, member, snap, &mut unsupported);
                let signature = serde_json::to_string(&slot).expect("finite skill shape");
                let next = intern.len();
                let id = *intern.entry(signature).or_insert(next);
                ids.push(id);
                slots.push(slot);
            }
        }
        let judgements = live::judgements(chart.times.len(), play);
        let input = chart::prepare(master, chart, 0, Vec::new(), judgements, 1.0, 100.0);
        let mut raise = 0.0_f64;
        let mut recovery = 0;
        let mut longest = 0;
        for slot in &slots {
            if input.judgements.is_some() {
                raise = raise.max(
                    slot.effects
                        .iter()
                        .fold(0.0, |sum, e| sum + e.delta as f64 / 100000.0),
                );
                recovery = recovery.max(slot.recovery);
            }
            for effect in &slot.effects {
                longest = longest.max(window_ms(effect.seconds, slot.extension_ms));
            }
        }
        let mut overlap = 1;
        for &time in &input.skill_times {
            overlap = overlap.max(
                input
                    .skill_times
                    .iter()
                    .filter(|&&other| other <= time && time < other + longest)
                    .count(),
            );
        }
        let factor_cap = 1.0 + overlap as f64 * raise;
        let high = if let Some(j) = &input.judgements {
            model::linear_play(&input, j, true, 0, 1.0, recovery)
        } else {
            model::linear(&input)
        };
        let low = if let Some(j) = &input.judgements {
            model::linear_play(&input, j, false, 0, 1.0, 0)
        } else {
            high.clone()
        };
        let play_bounds = input
            .judgements
            .as_ref()
            .map(|j| PlayBounds::new(&input, j));
        let drains = play_bounds
            .as_ref()
            .is_some_and(|b| b.damage.iter().fold(0.0, |sum, r| sum + r.1) >= input.life_base);
        Self {
            input,
            high,
            low,
            factor_cap,
            max_recovery: recovery,
            drains,
            slots,
            ids,
            width,
            weights: HashMap::new(),
            team_models: HashMap::new(),
            play_bounds,
            scores: HashMap::new(),
            score_order: VecDeque::new(),
        }
    }
    fn at(&self, member: usize, snap: i32) -> usize {
        member * self.width + (snap + 1) as usize
    }
    pub fn slot(&self, member: usize, snap: i32) -> &Slot {
        &self.slots[self.at(member, snap)]
    }
    pub(crate) fn weight(&mut self, member: usize, snap: i32) -> Pair {
        let index = self.at(member, snap);
        let id = self.ids[index];
        self.weights
            .entry(id)
            .or_insert_with(|| {
                let skill = &self.slots[index];
                let mut upper = model::skill_weights(&self.high, &self.input, skill);
                if let Some(j) = &self.input.judgements {
                    if skill.convert.is_some() {
                        let extra =
                            model::conversion_weights(&self.input, j, skill, self.factor_cap);
                        for (event, &value) in extra.iter().enumerate() {
                            upper.per_event[event] += value;
                        }
                        upper.mean = upper.per_event.iter().fold(0.0, |sum, v| sum + v) / 5.0;
                        upper.best = upper
                            .per_event
                            .iter()
                            .copied()
                            .fold(f64::NEG_INFINITY, f64::max);
                    }
                }
                let lower = if self.input.judgements.is_none() {
                    upper.clone()
                } else {
                    model::skill_weights(&self.low, &self.input, skill)
                };
                Pair {
                    high: upper,
                    low: lower,
                }
            })
            .clone()
    }
    pub fn score_high(&self, t: Totals) -> f64 {
        t.power * (self.high.total + t.skill) / self.input.converted_count as f64
            * (1.0 + RELATIVE_ERROR)
    }
    pub fn score_low(&self, t: Totals) -> f64 {
        (t.power * (self.low.total + t.skill_low) / self.input.converted_count as f64
            * (1.0 - RELATIVE_ERROR)
            - self.low.absolute_error)
            .max(0.0)
    }
    pub fn evaluate(&mut self, team: &Team, power: i32) -> Result<Arc<Scores>, String> {
        let ids: [usize; 5] =
            std::array::from_fn(|slot| self.ids[self.at(team.members[slot], team.snaps[slot])]);
        let key = (power, ids);
        if let Some(scores) = self.scores.get(&key) {
            return Ok(Arc::clone(scores));
        }
        self.input.power = power;
        self.input.slots = team
            .members
            .iter()
            .enumerate()
            .map(|(slot, &m)| self.slot(m, team.snaps[slot]).clone())
            .collect();
        let scores = Arc::new(live::score(&self.input)?);
        if self.scores.len() >= 4096 {
            if let Some(old) = self.score_order.pop_front() {
                self.scores.remove(&old);
            }
        }
        self.score_order.push_back(key);
        self.scores.insert(key, Arc::clone(&scores));
        Ok(scores)
    }
    pub fn order_bounds(&mut self, team: &Team, power: f64) -> (Vec<f64>, Vec<f64>) {
        let mut lows = vec![0.0; 120];
        let mut highs = vec![0.0; 120];
        if let Some(mut bounds) = self.play_bounds.take() {
            let skills: Vec<_> = team
                .members
                .iter()
                .enumerate()
                .map(|(slot, &m)| self.slot(m, team.snaps[slot]).clone())
                .collect();
            let ids: Vec<_> = team
                .members
                .iter()
                .enumerate()
                .map(|(slot, &m)| self.ids[self.at(m, team.snaps[slot])])
                .collect();
            let j = self.input.judgements.as_ref().unwrap();
            for (index, order) in live::orders().iter().enumerate() {
                let (low, high) = bounds.order(&self.input, j, &skills, &ids, order, power);
                lows[index] = low;
                highs[index] = high;
            }
            self.play_bounds = Some(bounds);
        } else {
            let weights: Vec<_> = team
                .members
                .iter()
                .enumerate()
                .map(|(slot, &m)| self.weight(m, team.snaps[slot]))
                .collect();
            for (index, order) in live::orders().iter().enumerate() {
                let (mut low, mut high) = (0.0, 0.0);
                for (event, &slot) in order.iter().enumerate() {
                    low += weights[slot].low.per_event[event];
                    high += weights[slot].high.per_event[event];
                }
                lows[index] = self.score_low(Totals {
                    power,
                    skill_low: low,
                    ..Totals::default()
                });
                highs[index] = self.score_high(Totals {
                    power,
                    skill: high,
                    ..Totals::default()
                });
            }
        }
        (lows, highs)
    }
    pub fn team_high(&mut self, team: &Team, t: Totals) -> f64 {
        let Some(j) = &self.input.judgements else {
            return self.score_high(t);
        };
        let recovery = team
            .members
            .iter()
            .enumerate()
            .map(|(slot, &m)| self.slot(m, team.snaps[slot]).recovery)
            .max()
            .unwrap_or(0)
            .max(0);
        if recovery >= self.max_recovery {
            return self.score_high(t);
        }
        let entry = self.team_models.entry(recovery).or_insert_with(|| {
            (
                model::linear_play(&self.input, j, true, 0, 1.0, recovery),
                HashMap::new(),
            )
        });
        let mut skill = 0.0;
        for (slot, &member) in team.members.iter().enumerate() {
            let index = member * self.width + (team.snaps[slot] + 1) as usize;
            let id = self.ids[index];
            skill += *entry.1.entry(id).or_insert_with(|| {
                let value = &self.slots[index];
                let mut weights = model::skill_weights(&entry.0, &self.input, value);
                if value.convert.is_some() {
                    let extra = model::conversion_weights(&self.input, j, value, self.factor_cap);
                    for (event, &value) in extra.iter().enumerate() {
                        weights.per_event[event] += value;
                    }
                }
                weights
                    .per_event
                    .iter()
                    .copied()
                    .fold(f64::NEG_INFINITY, f64::max)
            });
        }
        t.power * (entry.0.total + skill) / self.input.converted_count as f64
            * (1.0 + RELATIVE_ERROR)
    }
}
pub struct LiveObjective {
    pub core: LiveCore,
    pub criterion: Criterion,
}
impl Objective for LiveObjective {
    fn skill(&mut self, member: usize, snap: i32) -> (f64, f64) {
        let w = self.core.weight(member, snap);
        match self.criterion {
            Criterion::Min => (w.low.worst, w.high.mean),
            Criterion::Max => (w.low.mean, w.high.best),
            Criterion::Mean => (w.low.mean, w.high.mean),
        }
    }
    fn bound(&mut self, t: Totals) -> f64 {
        self.core.score_high(t)
    }
    fn product_base(&self) -> Option<f64> {
        Some(self.core.high.total)
    }
    fn leaf_bound(&mut self, team: &Team, t: Totals) -> Option<f64> {
        if self.core.input.judgements.is_some() {
            Some(self.core.team_high(team, t))
        } else {
            None
        }
    }
    fn prefer_snap(&self, member: usize, snap: i32) -> f64 {
        if self.core.drains {
            self.core.slot(member, snap).recovery as f64
        } else {
            0.0
        }
    }
    fn interval(&mut self, team: &Team, t: Totals) -> (f64, f64) {
        if self.criterion == Criterion::Mean && self.core.input.judgements.is_none() {
            return (self.core.score_low(t), self.core.score_high(t));
        }
        let (lows, highs) = self.core.order_bounds(team, t.power);
        match self.criterion {
            Criterion::Mean => (
                lows.iter().fold(0.0, |sum, v| sum + v) / 120.0,
                highs.iter().fold(0.0, |sum, v| sum + v) / 120.0,
            ),
            Criterion::Min => (
                lows.into_iter().fold(f64::INFINITY, f64::min),
                highs.into_iter().fold(f64::INFINITY, f64::min),
            ),
            Criterion::Max => (
                lows.into_iter().fold(f64::NEG_INFINITY, f64::max),
                highs.into_iter().fold(f64::NEG_INFINITY, f64::max),
            ),
        }
    }
    fn exact(&mut self, team: &Team, power: f64, _slots: &[f64]) -> Result<Exact, String> {
        let scores = self.core.evaluate(team, power as i32)?;
        Ok(Exact {
            key: self.criterion.key(&scores),
            detail: json!({"scores":&*scores}),
        })
    }
}
pub struct Potential {
    areas: Vec<f64>,
    width: usize,
    window: f64,
}
impl Potential {
    pub fn new(master: &Master, members: &[Member], snaps: &[Snap], window: f64) -> Self {
        let width = snaps.len() + 1;
        let mut areas = Vec::with_capacity(members.len() * width);
        let mut unsupported = Vec::new();
        for member in members {
            for snap in std::iter::once(None).chain(snaps.iter().map(Some)) {
                let skill = skills::resolve(master, member, snap, &mut unsupported);
                let mut area = 0.0;
                for effect in &skill.effects {
                    if (effect.kind == 2000 || effect.judgements.contains(&5))
                        && !effect.gate.blocks(master.live.life_base)
                    {
                        area += effect.delta as f64 / 100000.0
                            * (window_ms(effect.seconds, skill.extension_ms) as f64 / 1000.0);
                    }
                }
                areas.push(area);
            }
        }
        Self {
            areas,
            width,
            window,
        }
    }
    fn area(&self, member: usize, snap: i32) -> f64 {
        self.areas[member * self.width + (snap + 1) as usize]
    }
    fn key(&self, power: f64, area: f64) -> f64 {
        power * (1.0 + area / self.window)
    }
}
impl Objective for Potential {
    fn skill(&mut self, member: usize, snap: i32) -> (f64, f64) {
        let area = self.area(member, snap);
        (area, area)
    }
    fn bound(&mut self, t: Totals) -> f64 {
        self.key(t.power, t.skill) * (1.0 + 1e-12)
    }
    fn interval(&mut self, _team: &Team, t: Totals) -> (f64, f64) {
        (self.key(t.power, t.skill_low), self.key(t.power, t.skill))
    }
    fn exact(&mut self, team: &Team, power: f64, _slots: &[f64]) -> Result<Exact, String> {
        let area = team
            .members
            .iter()
            .enumerate()
            .fold(0.0, |sum, (slot, &m)| sum + self.area(m, team.snaps[slot]));
        let value = self.key(power, area);
        Ok(Exact {
            key: value,
            detail: json!({"potential":value,"area":area}),
        })
    }
}

pub struct ScoreCap {
    pub core: LiveCore,
}
impl Objective for ScoreCap {
    fn skill(&mut self, m: usize, s: i32) -> (f64, f64) {
        let best = self.core.weight(m, s).high.best;
        (best, best)
    }
    fn bound(&mut self, t: Totals) -> f64 {
        self.core.score_high(t)
    }
    fn product_base(&self) -> Option<f64> {
        Some(self.core.high.total)
    }
    fn interval(&mut self, _team: &Team, t: Totals) -> (f64, f64) {
        let value = self.core.score_high(t);
        (value, value)
    }
    fn exact(&mut self, team: &Team, power: f64, _slots: &[f64]) -> Result<Exact, String> {
        let skill = team
            .members
            .iter()
            .enumerate()
            .map(|(slot, &m)| self.core.weight(m, team.snaps[slot]).high.best)
            .sum();
        Ok(Exact {
            key: self.core.score_high(Totals {
                power,
                skill,
                ..Totals::default()
            }),
            detail: Value::Null,
        })
    }
}
