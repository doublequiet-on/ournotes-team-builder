//! Event settlement and ordinary live reward bounds, shared by native and WASM.
use crate::{
    chart::{self, Chart},
    master::{EventEffect, Master, Reward},
    model::RELATIVE_ERROR,
    num::{i32, imul},
    objectives::LiveCore,
    power::{self, Member, Snap},
    search::{Exact, JointTarget, Objective, Team, Totals},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, PartialEq)]
pub enum RouteKind {
    Live,
    Challenge,
    Skip,
}
#[derive(Clone)]
pub struct Route {
    pub kind: RouteKind,
    pub rate: f64,
    pub reward_rate: f64,
    pub points: BTreeMap<i32, i32>,
    pub rewards: Vec<Reward>,
    pub item_id: i32,
    pub challenge_points: BTreeMap<i32, i32>,
    pub effects: Vec<EventEffect>,
    /// The tables above by score rank, read on every bound.
    ranks: Vec<RankRow>,
}
#[derive(Clone, Default)]
struct RankRow {
    points: i32,
    challenge_points: i32,
    /// Item count and probability of each matching reward row, in table order.
    items: Vec<(i32, f64)>,
}
impl Route {
    pub fn new(
        master: &Master,
        event_id: i32,
        kind: RouteKind,
        consumption: i32,
    ) -> Result<Self, String> {
        let event = master
            .events
            .get(&event_id)
            .ok_or_else(|| format!("unknown-event:{event_id}"))?;
        let challenge =
            kind == RouteKind::Challenge || (kind == RouteKind::Skip && consumption > 10);
        if !challenge && ![0, 1, 2, 3, 4, 5, 10].contains(&consumption) {
            return Err("unsupported-live-boost-cost".into());
        }
        let table = if challenge {
            &master.challenge_boosts
        } else {
            &master.boosts
        };
        let boost = table.iter().find(|row| row.consumed == consumption);
        let rate = if !challenge && consumption < 1 {
            1.0
        } else {
            boost.map(|r| r.event_point_rate).unwrap_or(1.0)
        };
        let reward_rate = if !challenge && consumption < 1 {
            1.0
        } else {
            boost.map(|r| r.reward_rate).unwrap_or(1.0)
        };
        let mut route = Self {
            kind,
            rate,
            reward_rate,
            points: if challenge {
                event.challenge_points.clone()
            } else {
                event.live_points.clone()
            },
            rewards: if challenge {
                event.challenge_rewards.clone()
            } else {
                event.live_rewards.clone()
            },
            item_id: event.item_id,
            challenge_points: (*master.live_challenge_points).clone(),
            effects: event.effects.clone(),
            ranks: Vec::new(),
        };
        let top = route
            .points
            .keys()
            .chain(route.challenge_points.keys())
            .copied()
            .chain(route.rewards.iter().map(|r| r.score_rank))
            .max()
            .unwrap_or(0)
            .clamp(0, 64);
        route.ranks = (0..=top)
            .map(|rank| RankRow {
                points: *route.points.get(&rank).unwrap_or(&0),
                challenge_points: *route.challenge_points.get(&rank).unwrap_or(&0),
                items: route
                    .rewards
                    .iter()
                    .filter(|r| {
                        r.score_rank == rank
                            && (route.item_id == 0 || r.resource_id == route.item_id)
                    })
                    .map(|r| (r.count, r.probability))
                    .collect(),
            })
            .collect();
        Ok(route)
    }
    fn rank_row(&self, rank: i32) -> Option<&RankRow> {
        usize::try_from(rank).ok().and_then(|i| self.ranks.get(i))
    }
    pub fn points(&self, bonus: f64, rank: i32) -> f64 {
        let value = self.rank_row(rank).map_or(0, |row| row.points);
        let rate = i32(self.rate);
        let bonus = i32(bonus + 10000.0);
        let product = if self.kind == RouteKind::Challenge {
            imul(imul(value, rate), bonus)
        } else {
            imul(imul(bonus, rate), value)
        };
        (product as f64 / 10000.0).trunc()
    }
    pub fn items(&self, bonus: f64, rank: i32) -> f64 {
        let Some(row) = self.rank_row(rank) else {
            return 0.0;
        };
        row.items.iter().fold(0.0, |sum, &(count, probability)| {
            sum + (imul(imul(count, i32(bonus + 10000.0)), i32(self.reward_rate)) as f64 / 10000.0)
                .trunc()
                * probability
                / 10000.0
        })
    }
    pub fn cp(&self, rank: i32) -> f64 {
        if self.kind == RouteKind::Live {
            self.rank_row(rank).map_or(0, |row| row.challenge_points) as f64 * self.rate
        } else {
            0.0
        }
    }
}
fn top_five(values: impl Iterator<Item = f64>) -> f64 {
    let mut rows: Vec<_> = values.collect();
    rows.sort_by(|a, b| b.total_cmp(a));
    rows.into_iter().take(5).sum()
}
fn best_order(rows: &[[f64; 5]]) -> f64 {
    let mut dp = [f64::NEG_INFINITY; 32];
    dp[0] = 0.0;
    for mask in 0usize..31 {
        if dp[mask] == f64::NEG_INFINITY {
            continue;
        }
        let row = mask.count_ones() as usize;
        for (column, &weight) in rows[row].iter().enumerate() {
            let bit = 1 << column;
            if mask & bit == 0 {
                dp[mask | bit] = dp[mask | bit].max(dp[mask] + weight);
            }
        }
    }
    dp[31]
}
pub struct EventObjective {
    pub core: Option<LiveCore>,
    pub chart: Option<Chart>,
    pub route: Route,
    pub points_measure: bool,
    pub skip_rank: i32,
    pub cp_weight: f64,
    pub exchange: Option<(f64, f64)>,
    pub score_cap: f64,
    member_bonus: Vec<f64>,
    snap_bonus: Vec<f64>,
    other_member: Vec<f64>,
    other_snap: Vec<f64>,
    tie: f64,
    max_other: f64,
    max_bonus: f64,
    suffix: [f64; 6],
    ordered: Vec<usize>,
    targets: HashMap<u64, Vec<JointTarget>>,
}
impl EventObjective {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        master: &Master,
        members: &[Member],
        snaps: &[Snap],
        chart: Option<&Chart>,
        route: Route,
        points_measure: bool,
        play: &Value,
        primary_only: bool,
        cp_weight: f64,
        exchange: Option<(f64, f64)>,
        score_cap: f64,
    ) -> Self {
        let kind = if points_measure { 0 } else { 1 };
        let member_bonus: Vec<_> = members
            .iter()
            .map(|m| power::member_event_percent(&route.effects, m, kind) as f64)
            .collect();
        let snap_bonus: Vec<_> = snaps
            .iter()
            .map(|s| power::snap_event_percent(&route.effects, s, kind) as f64)
            .collect();
        let other_member: Vec<_> = members
            .iter()
            .map(|m| power::member_event_percent(&route.effects, m, 1 - kind) as f64)
            .collect();
        let other_snap: Vec<_> = snaps
            .iter()
            .map(|s| power::snap_event_percent(&route.effects, s, 1 - kind) as f64)
            .collect();
        let tie = if primary_only { 0.0 } else { 1.0 / 16777216.0 };
        let max_other = top_five(other_member.iter().map(|v| v.max(0.0)))
            + top_five(other_snap.iter().map(|v| v.max(0.0)));
        let max_bonus = if primary_only {
            top_five(member_bonus.iter().copied()) + top_five(snap_bonus.iter().copied())
        } else {
            1000000.0
        };
        let played = route.kind != RouteKind::Skip;
        Self {
            core: chart
                .filter(|_| played)
                .map(|c| LiveCore::new(master, c, members, snaps, play)),
            chart: chart.filter(|_| played).cloned(),
            route,
            points_measure,
            skip_rank: master.params.skip_rank,
            cp_weight,
            exchange,
            score_cap,
            member_bonus,
            snap_bonus,
            other_member,
            other_snap,
            tie,
            max_other,
            max_bonus,
            suffix: [0.0; 6],
            ordered: Vec::new(),
            targets: HashMap::new(),
        }
    }
    /// Ranks by `measure + cp·CP + other·(the other reward)`, the weights already divided by the measure's weight.
    pub fn with_weights(mut self, cp: f64, other: f64) -> Self {
        self.cp_weight = cp;
        self.exchange = None;
        self.tie = other;
        self.max_other = top_five(self.other_member.iter().map(|v| v.max(0.0)))
            + top_five(self.other_snap.iter().map(|v| v.max(0.0)));
        self
    }
    fn direct(&self, bonus: f64, rank: i32) -> f64 {
        if self.points_measure {
            self.route.points(bonus, rank)
        } else {
            self.route.items(bonus, rank)
        }
    }
    fn other(&self, bonus: f64, rank: i32) -> f64 {
        if self.points_measure {
            self.route.items(bonus, rank)
        } else {
            self.route.points(bonus, rank)
        }
    }
    fn value(&self, bonus: f64, rank: i32) -> f64 {
        let direct = self.direct(bonus, rank);
        let cp = self.route.cp(rank);
        if let Some((n, d)) = self.exchange {
            direct * d + cp * n
        } else {
            direct + self.cp_weight * cp
        }
    }
    fn reward_bound(&self, bonus: f64, rank: i32) -> f64 {
        self.value(bonus, rank) + self.tie * self.other(self.max_other, rank)
    }
    fn other_snap(&self, snap: i32) -> f64 {
        if snap < 0 {
            0.0
        } else {
            self.other_snap[snap as usize]
        }
    }
    fn other_bonus(&self, team: &Team) -> f64 {
        team.members
            .iter()
            .map(|&m| self.other_member[m])
            .sum::<f64>()
            + team.snaps.iter().map(|&s| self.other_snap(s)).sum::<f64>()
    }
    fn team_bonus(&self, team: &Team) -> f64 {
        team.members
            .iter()
            .map(|&m| self.member_bonus[m])
            .sum::<f64>()
            + team
                .snaps
                .iter()
                .filter(|&&s| s >= 0)
                .map(|&s| self.snap_bonus[s as usize])
                .sum::<f64>()
    }
    fn rank(&self, score: f64) -> i32 {
        chart::score_rank(self.chart.as_ref().unwrap(), score)
    }
    fn rank_bound(&self, score: f64) -> i32 {
        self.rank(score.min(self.score_cap))
    }
}
impl Objective for EventObjective {
    fn skill(&mut self, member: usize, snap: i32) -> (f64, f64) {
        if let Some(core) = &mut self.core {
            let w = core.weight(member, snap);
            (w.low.worst, w.high.best)
        } else {
            let bonus = if self.tie != 0.0 {
                self.other_member[member] + self.other_snap(snap)
            } else {
                0.0
            };
            (bonus, bonus)
        }
    }
    fn member_bonus(&self, member: usize) -> f64 {
        self.member_bonus[member]
    }
    fn snap_bonus(&self, snap: usize) -> f64 {
        self.snap_bonus[snap]
    }
    fn snap_dominance(&mut self, _member: usize, snap: i32) -> Vec<f64> {
        if self.tie != 0.0 {
            vec![self.other_snap(snap)]
        } else {
            Vec::new()
        }
    }
    fn product_base(&self) -> Option<f64> {
        self.core.as_ref().map(|c| c.high.total)
    }
    fn bound(&mut self, t: Totals) -> f64 {
        if let Some(core) = &self.core {
            self.reward_bound(t.bonus, self.rank_bound(core.score_high(t)))
        } else {
            self.value(t.bonus, self.skip_rank) + self.tie * self.other(t.skill, self.skip_rank)
        }
    }
    fn joint_targets(&mut self, limit: f64) -> Option<Vec<JointTarget>> {
        let core = self.core.as_ref()?;
        let scale = core.input.converted_count as f64 / (1.0 + RELATIVE_ERROR);
        if let Some(rows) = self.targets.get(&limit.to_bits()) {
            return Some(rows.clone());
        }
        let chart = self.chart.as_ref().unwrap();
        let ranks = std::iter::once((2, 0.0)).chain(
            chart
                .ranks
                .iter()
                .filter(|r| r.rank > 2)
                .map(|r| (r.rank, r.required as f64)),
        );
        let mut targets = Vec::new();
        for (rank, required) in ranks {
            if required > self.score_cap || self.reward_bound(self.max_bonus, rank) < limit {
                continue;
            }
            let (mut low, mut high) = (0, self.max_bonus as i64);
            while low < high {
                let mid = (low + high) >> 1;
                if self.reward_bound(mid as f64, rank) >= limit {
                    high = mid
                } else {
                    low = mid + 1
                }
            }
            targets.push(JointTarget {
                bonus: low as f64,
                product: required * scale,
            });
        }
        if self.targets.len() > 64 {
            self.targets.clear();
        }
        self.targets.insert(limit.to_bits(), targets.clone());
        Some(targets)
    }
    fn bonus_floor(&mut self, limit: f64) -> Option<f64> {
        self.joint_targets(limit).map(|targets| {
            targets
                .iter()
                .map(|t| t.bonus)
                .fold(f64::INFINITY, f64::min)
        })
    }
    fn prepare_assignment(&mut self, _members: &[usize], options: &[Vec<i32>]) {
        if self.tie == 0.0 || self.core.is_none() { return; }
        self.suffix = [0.0; 6];
        for slot in (0..5).rev() {
            self.suffix[slot] = self.suffix[slot + 1]
                + options[slot]
                    .iter()
                    .map(|&s| self.other_snap(s))
                    .fold(f64::NEG_INFINITY, f64::max);
        }
        self.ordered = options
            .iter()
            .flatten()
            .filter(|&&s| s >= 0 && self.other_snap(s) > 0.0)
            .map(|&s| s as usize)
            .collect();
        self.ordered.sort_unstable();
        self.ordered.dedup();
        self.ordered
            .sort_by(|&a, &b| self.other_snap[b].total_cmp(&self.other_snap[a]));
    }
    fn assignment_bound(
        &mut self,
        members: &[usize],
        t: Totals,
        assigned: &[i32],
        from: usize,
    ) -> Option<f64> {
        if self.tie == 0.0 || self.core.is_none() {
            return None;
        }
        let rank = self.rank_bound(self.core.as_ref().unwrap().score_high(t));
        let distinct = self
            .ordered
            .iter()
            .filter(|&&s| !assigned[..from].contains(&(s as i32)))
            .take(5 - from)
            .map(|&s| self.other_snap[s])
            .sum::<f64>();
        let bonus = members.iter().map(|&m| self.other_member[m]).sum::<f64>()
            + self.suffix[from].min(distinct)
            + assigned[..from]
                .iter()
                .map(|&s| self.other_snap(s))
                .sum::<f64>();
        Some(self.value(t.bonus, rank) + self.tie * self.other(bonus, rank))
    }
    fn leaf_bound(&mut self, team: &Team, t: Totals) -> Option<f64> {
        let other = self.other_bonus(team);
        if let Some(core) = &mut self.core {
            let score = if core.input.judgements.is_some() {
                core.team_high(team, t)
            } else {
                let weights: Vec<_> = team
                    .members
                    .iter()
                    .enumerate()
                    .map(|(slot, &m)| core.weight(m, team.snaps[slot]).high.per_event)
                    .collect();
                core.score_high(Totals {
                    skill: best_order(&weights),
                    ..t
                })
            };
            let rank = self.rank_bound(score);
            Some(self.value(t.bonus, rank) + self.tie * self.other(other, rank))
        } else {
            Some(self.value(t.bonus, self.skip_rank) + self.tie * self.other(other, self.skip_rank))
        }
    }
    fn interval(&mut self, team: &Team, t: Totals) -> (f64, f64) {
        let other = self.other_bonus(team);
        let Some(core) = &self.core else {
            let key =
                self.value(t.bonus, self.skip_rank) + self.tie * self.other(other, self.skip_rank);
            return (key, key);
        };
        let low_rank = self.rank(core.score_low(t));
        let high_rank = self.rank_bound(core.score_high(t));
        if low_rank == high_rank {
            let key = self.value(t.bonus, low_rank) + self.tie * self.other(other, low_rank);
            return (key, key);
        }
        let (lows, highs) = self.core.as_mut().unwrap().order_bounds(team, t.power);
        let (mut low, mut high) = (0.0, 0.0);
        for (&a, &b) in lows.iter().zip(&highs) {
            let r = self.rank(a);
            let s = self.rank_bound(b);
            if r != s {
                return match self.exact(team, t.power, &[]) {
                    Ok(result) => (result.key, result.key),
                    Err(_) => (f64::NEG_INFINITY, f64::INFINITY),
                };
            }
            low += self.value(t.bonus, r) + self.tie * self.other(other, r);
            high += self.value(t.bonus, s) + self.tie * self.other(other, s);
        }
        (low / lows.len() as f64, high / highs.len() as f64)
    }
    fn exact(&mut self, team: &Team, power: f64, _slots: &[f64]) -> Result<Exact, String> {
        let bonus = self.team_bonus(team);
        let other = self.other_bonus(team);
        let Some(core) = &mut self.core else {
            let mean = self.value(bonus, self.skip_rank);
            let direct = self.direct(bonus, self.skip_rank);
            let other_mean = self.other(other, self.skip_rank);
            return Ok(Exact {
                key: mean + self.tie * other_mean,
                detail: json!({"mean":mean,"bonus":bonus,"directMean":direct,"otherMean":other_mean,"directMin":direct,"directMax":direct}),
            });
        };
        let scores = core.evaluate(team, power as i32)?;
        let mut per_order = Vec::with_capacity(scores.scores.len());
        let mut ranks = BTreeMap::<i32, i32>::new();
        let (mut total, mut cp, mut reward_sum, mut direct_min, mut direct_max, mut other_sum) =
            (0.0, 0.0, 0.0, f64::INFINITY, f64::NEG_INFINITY, 0.0);
        for &score in &scores.scores {
            let rank = self.rank(score);
            *ranks.entry(rank).or_default() += 1;
            let value = self.value(bonus, rank);
            per_order.push(value);
            total += value;
            cp += self.route.cp(rank);
            let reward = self.direct(bonus, rank);
            reward_sum += reward;
            direct_min = direct_min.min(reward);
            direct_max = direct_max.max(reward);
            other_sum += self.other(other, rank);
        }
        let count = scores.scores.len() as f64;
        let mean = total / count;
        Ok(Exact {
            key: mean + self.tie * other_sum / count,
            detail: json!({"scores":&*scores,"perOrder":per_order,"mean":mean,"bonus":bonus,"ranks":ranks,"rewardSum":reward_sum,"comparisonSum":total,"directMin":direct_min,"directMax":direct_max,"directMean":reward_sum/count,"otherMean":other_sum/count,"challengePoints":cp/count}),
        })
    }
}

/// Refine an exact primary-reward group by the other reward, then mean score.
pub struct ScoreTie {
    pub event: EventObjective,
    pub mean: crate::objectives::LiveObjective,
    pub target: f64,
    pub excluded: Vec<Vec<usize>>,
    pub rank_other: bool,
    pub other_target: Option<f64>,
    other_suffix: [f64; 6],
    member_suffix: Vec<Vec<f64>>,
    other_snaps: f64,
    other_ordered: Vec<i32>,
    other_order: Vec<i32>,
    other_allowed: Vec<bool>,
    prepared_options: Vec<Vec<i32>>,
    weight_options: Vec<Vec<i32>>,
    cached_weights: Vec<f64>,
    suffix: [f64; 6],
    floor: Option<Option<f64>>,
}
impl ScoreTie {
    pub fn new(
        event: EventObjective,
        mean: crate::objectives::LiveObjective,
        target: f64,
        teams: &[Team],
    ) -> Self {
        let excluded = teams
            .iter()
            .map(|t| {
                let mut key = t.members.clone();
                key.sort_unstable();
                key
            })
            .collect();
        let other_snaps = top_five(event.other_snap.iter().copied());
        let mut other_order: Vec<_> = (0..event.other_snap.len()).map(|i| i as i32).collect();
        other_order.sort_by(|&a, &b| event.other_snap(b).total_cmp(&event.other_snap(a)));
        let other_allowed = vec![false; event.other_snap.len()];
        let weight_options = vec![Vec::new(); event.member_bonus.len()];
        let cached_weights = vec![f64::NEG_INFINITY; event.member_bonus.len()];
        Self {
            event,
            mean,
            target,
            excluded,
            rank_other: false,
            other_target: None,
            other_suffix: [0.0; 6],
            member_suffix: Vec::new(),
            other_snaps,
            other_ordered: Vec::new(),
            other_order,
            other_allowed,
            prepared_options: Vec::new(),
            weight_options,
            cached_weights,
            suffix: [0.0; 6],
            floor: None,
        }
    }
    fn other_upper(&self, t: Totals, bonus: f64) -> f64 {
        let rank = if self.rank_other { self.event.rank_bound(self.event.core.as_ref().unwrap().score_high(t)) }
            else { self.event.rank_bound(f64::INFINITY) };
        self.event.other(bonus, rank)
    }
    fn ranked_upper(&mut self, t: Totals, bonus: f64) -> f64 {
        let upper = self.other_upper(t, bonus);
        if self.other_target.is_some_and(|target| upper < target) { -1.0 }
        else if self.rank_other { upper } else { self.mean.bound(t) }
    }
    fn eligible(&self, team: &Team) -> bool {
        !self.excludes(&team.members)
    }
    fn excludes(&self, members: &[usize]) -> bool {
        if self.excluded.is_empty() {
            return false;
        }
        let mut key = [0; 5];
        let key = &mut key[..members.len()];
        key.copy_from_slice(members);
        key.sort_unstable();
        self.excluded.iter().any(|team| team[..] == key[..])
    }
    fn event_totals(&mut self, team: &Team, t: Totals) -> Totals {
        let (mut low, mut high) = (0.0, 0.0);
        for (slot, &member) in team.members.iter().enumerate() {
            let (a, b) = self.event.skill(member, team.snaps[slot]);
            low += a;
            high += b;
        }
        Totals {
            skill: high,
            skill_low: low,
            ..t
        }
    }
}
impl Objective for ScoreTie {
    fn preserve_snap_options(&self) -> bool {
        true
    }
    fn skill(&mut self, m: usize, s: i32) -> (f64, f64) {
        if self.rank_other { self.event.skill(m, s) } else { self.mean.skill(m, s) }
    }
    fn member_bonus(&self, m: usize) -> f64 {
        self.event.member_bonus(m)
    }
    fn snap_bonus(&self, s: usize) -> f64 {
        self.event.snap_bonus(s)
    }
    fn bound(&mut self, t: Totals) -> f64 {
        self.ranked_upper(t, self.event.max_other)
    }
    fn prepare_members(&mut self, characters: &[Vec<usize>]) {
        self.member_suffix = crate::search::suffix_tops(&characters.iter().map(|members| members.iter().map(|&m| self.event.other_member[m]).fold(f64::NEG_INFINITY, f64::max)).collect::<Vec<_>>());
    }
    fn member_bound_at(&mut self, members: &[usize], t: Totals, left: usize, from: usize) -> Option<f64> {
        if self.bonus_floor(f64::NEG_INFINITY).is_some_and(|floor| t.bonus < floor - 1e-6) { return Some(-1.0); }
        let bonus = members.iter().map(|&m| self.event.other_member[m]).sum::<f64>()
            + self.member_suffix[from][left] + self.other_snaps;
        Some(self.ranked_upper(t, bonus))
    }
    fn product_base(&self) -> Option<f64> { self.mean.product_base() }
    fn joint_targets(&mut self, _limit: f64) -> Option<Vec<JointTarget>> {
        if self.rank_other { self.event.joint_targets(self.target) } else { None }
    }
    fn bonus_floor(&mut self, _limit: f64) -> Option<f64> {
        let target = self.target;
        let event = &mut self.event;
        *self.floor.get_or_insert_with(|| {
            event.joint_targets(target).map(|targets| {
                targets
                    .iter()
                    .map(|t| t.bonus)
                    .fold(f64::INFINITY, f64::min)
            })
        })
    }
    fn prepare_assignment(&mut self, members: &[usize], options: &[Vec<i32>]) {
        if self.prepared_options != options {
            self.event.prepare_assignment(members, options);
            self.other_allowed.fill(false);
            for &s in options.iter().flatten() { if s >= 0 { self.other_allowed[s as usize] = true; } }
            self.other_ordered.clear();
            self.other_ordered.extend(self.other_order.iter().copied().filter(|&s| self.other_allowed[s as usize]));
            self.other_suffix = [0.0; 6];
            for slot in (0..5).rev() {
                self.other_suffix[slot] = self.other_suffix[slot + 1] + options[slot].iter().map(|&s| self.event.other_snap(s)).fold(f64::NEG_INFINITY, f64::max);
            }
            self.prepared_options = options.to_vec();
        }
        self.suffix = [0.0; 6];
        for slot in (0..5).rev() {
            let m = members[slot];
            if self.weight_options[m] != options[slot] {
                self.cached_weights[m] = options[slot].iter().map(|&s| self.event.skill(m, s).1).fold(f64::NEG_INFINITY, f64::max);
                self.weight_options[m] = options[slot].clone();
            }
            self.suffix[slot] = self.suffix[slot + 1] + self.cached_weights[m];
        }
    }
    fn assignment_bound(
        &mut self,
        members: &[usize],
        t: Totals,
        assigned: &[i32],
        from: usize,
    ) -> Option<f64> {
        if self.excludes(members) {
            return Some(-1.0);
        }
        let mut skill = self.suffix[from];
        for slot in 0..from {
            skill += self.event.skill(members[slot], assigned[slot]).1;
        }
        let optimistic = Totals { skill, ..t };
        let bound = self
            .event
            .assignment_bound(members, optimistic, assigned, from)
            .unwrap_or_else(|| self.event.bound(optimistic));
        let distinct = self.other_ordered.iter().filter(|s| !assigned[..from].contains(s)).take(5 - from).map(|&s| self.event.other_snap(s)).sum::<f64>();
        let bonus = members.iter().map(|&m| self.event.other_member[m]).sum::<f64>()
            + assigned.iter().take(from).map(|&s| self.event.other_snap(s)).sum::<f64>() + self.other_suffix[from].min(distinct);
        Some(if bound < self.target { -1.0 } else { self.ranked_upper(t, bonus) })
    }
    fn leaf_bound(&mut self, team: &Team, t: Totals) -> Option<f64> {
        if !self.eligible(team) {
            return Some(-1.0);
        }
        let totals = self.event_totals(team, t);
        let bound = self
            .event
            .leaf_bound(team, totals)
            .unwrap_or_else(|| self.event.bound(totals));
        Some(if bound < self.target { -1.0 } else { self.ranked_upper(t, self.event.other_bonus(team)) })
    }
    fn interval(&mut self, team: &Team, t: Totals) -> (f64, f64) {
        if !self.eligible(team) {
            return (-1.0, -1.0);
        }
        let totals = self.event_totals(team, t);
        let (low, high) = self.event.interval(team, totals);
        if high < self.target || low > self.target {
            return (-1.0, -1.0);
        }
        let bonus = self.event.other_bonus(team);
        let (lows, highs) = self.event.core.as_mut().unwrap().order_bounds(team, t.power);
        let other_low = lows.iter().map(|&v| self.event.other(bonus, self.event.rank(v))).sum::<f64>() / 120.0;
        let other_high = highs.iter().map(|&v| self.event.other(bonus, self.event.rank_bound(v))).sum::<f64>() / 120.0;
        if self.other_target.is_some_and(|target| other_low > target || other_high < target) { return (-1.0, -1.0); }
        if self.rank_other { return (if low == self.target && high == self.target { other_low } else { -1.0 }, other_high); }
        let (a, b) = self.mean.interval(team, t);
        (
            if low == self.target && high == self.target && self.other_target.is_none_or(|target| other_low == target && other_high == target) {
                a
            } else {
                -1.0
            },
            b,
        )
    }
    fn exact(&mut self, team: &Team, power: f64, slots: &[f64]) -> Result<Exact, String> {
        let result = self.event.exact(team, power, slots)?;
        let other = crate::master::number(&result.detail["otherMean"], 0.0);
        let key = if self.eligible(team) && result.key == self.target && self.other_target.is_none_or(|target| target == other) {
            if self.rank_other { other } else { crate::master::number(&result.detail["scores"]["mean"], 0.0) }
        } else {
            -1.0
        };
        Ok(Exact {
            key,
            detail: result.detail,
        })
    }
}

pub struct SkipTie {
    pub event: EventObjective,
    pub target: f64,
}
impl Objective for SkipTie {
    fn preserve_snap_options(&self) -> bool { true }
    fn skill(&mut self, m: usize, s: i32) -> (f64, f64) {
        let bonus = self.event.other_member[m] + self.event.other_snap(s);
        (bonus, bonus)
    }
    fn member_bonus(&self, m: usize) -> f64 { self.event.member_bonus[m] }
    fn snap_bonus(&self, s: usize) -> f64 { self.event.snap_bonus[s] }
    fn bound(&mut self, t: Totals) -> f64 {
        if self.event.reward_bound(t.bonus, self.event.skip_rank) < self.target { -1.0 }
        else { self.event.other(t.skill, self.event.skip_rank) }
    }
    fn interval(&mut self, team: &Team, t: Totals) -> (f64, f64) {
        let primary = self.event.value(t.bonus, self.event.skip_rank) + self.event.tie * self.event.other(t.skill, self.event.skip_rank);
        let key = if primary == self.target { self.event.other(self.event.other_bonus(team), self.event.skip_rank) } else { -1.0 };
        (key, key)
    }
    fn exact(&mut self, team: &Team, power: f64, slots: &[f64]) -> Result<Exact, String> {
        let mut result = self.event.exact(team, power, slots)?;
        result.key = if result.key == self.target { crate::master::number(&result.detail["otherMean"], 0.0) } else { -1.0 };
        Ok(result)
    }
}

pub struct BonusSurrogate {
    pub members: Vec<f64>,
    pub snaps: Vec<f64>,
    pub weight: f64,
}
impl Objective for BonusSurrogate {
    fn skill(&mut self, _m: usize, _s: i32) -> (f64, f64) {
        (0.0, 0.0)
    }
    fn member_bonus(&self, m: usize) -> f64 {
        self.members[m]
    }
    fn snap_bonus(&self, s: usize) -> f64 {
        self.snaps[s]
    }
    fn bound(&mut self, t: Totals) -> f64 {
        t.bonus * self.weight + t.power
    }
    fn interval(&mut self, _team: &Team, t: Totals) -> (f64, f64) {
        let key = self.bound(t);
        (key, key)
    }
    fn exact(&mut self, team: &Team, power: f64, _slots: &[f64]) -> Result<Exact, String> {
        let bonus = team.members.iter().map(|&m| self.members[m]).sum::<f64>()
            + team
                .snaps
                .iter()
                .filter(|&&s| s >= 0)
                .map(|&s| self.snaps[s as usize])
                .sum::<f64>();
        Ok(Exact {
            key: bonus * self.weight + power,
            detail: Value::Null,
        })
    }
}
pub struct ChallengePoints {
    pub mean: crate::objectives::LiveObjective,
    pub chart: Chart,
    pub route: Route,
}
impl ChallengePoints {
    fn value(&self, score: f64) -> f64 {
        *self
            .route
            .challenge_points
            .get(&chart::score_rank(&self.chart, score))
            .unwrap_or(&0) as f64
            * self.route.rate
    }
}
impl Objective for ChallengePoints {
    fn skill(&mut self, m: usize, s: i32) -> (f64, f64) {
        self.mean.skill(m, s)
    }
    fn bound(&mut self, t: Totals) -> f64 {
        let score = self.mean.bound(t);
        self.value(score) + score / 68719476736.0
    }
    fn interval(&mut self, team: &Team, t: Totals) -> (f64, f64) {
        let (a, b) = self.mean.interval(team, t);
        (
            self.value(a) + a / 68719476736.0,
            self.value(b) + b / 68719476736.0,
        )
    }
    fn exact(&mut self, team: &Team, power: f64, slots: &[f64]) -> Result<Exact, String> {
        let result = self.mean.exact(team, power, slots)?;
        let scores = &result.detail["scores"];
        let per_order: Vec<_> = crate::master::rows(&scores["scores"])
            .into_iter()
            .map(|s| self.value(crate::master::number(s, 0.0)))
            .collect();
        let mean = per_order.iter().sum::<f64>() / per_order.len() as f64;
        Ok(Exact {
            key: mean + crate::master::number(&scores["mean"], 0.0) / 68719476736.0,
            detail: json!({"scores":scores,"perOrder":per_order,"mean":mean,"bonus":0,"challengePoints":mean}),
        })
    }
}
