//! Independent raw-JUST score programs provide a bound before complete replay.
use crate::{
    full::{conditions::Performer, model::Model},
    gekiso::{self, Context},
    master::Master,
    num::f,
};
use rustc_hash::FxHashMap as HashMap;
use std::sync::Arc;

pub(crate) fn slot_dominates(a: &crate::live::Slot, b: &crate::live::Slot) -> bool {
    if a.effects
        .iter()
        .chain(&b.effects)
        .any(|e| e.delta < 0 || e.gate.kind != 0)
        || a.convert.is_some()
        || b.convert.is_some()
    {
        return false;
    }
    let mut times = vec![0];
    for slot in [a, b] {
        times.extend(
            slot.effects
                .iter()
                .map(|e| crate::num::window_ms(e.seconds, slot.extension_ms)),
        );
    }
    times.sort_unstable();
    times.dedup();
    for time in times {
        for judgement in [5, 6] {
            let value = |slot: &crate::live::Slot| {
                slot.effects
                    .iter()
                    .filter(|e| {
                        crate::num::window_ms(e.seconds, slot.extension_ms) > time
                            && (e.kind == 2000 || e.judgements.contains(&judgement))
                    })
                    .map(|e| e.delta as i64)
                    .sum::<i64>()
            };
            if value(a) < value(b) {
                return false;
            }
        }
    }
    true
}

struct Effect {
    values: Vec<f64>,
    absolute: f64,
    commands: usize,
}
type Program = Arc<Vec<u64>>;
type ConversionCache = HashMap<Vec<usize>, (usize, Arc<Context>)>;
type WeightKey = (usize, Vec<usize>);
struct Basis {
    weights: Arc<crate::gekiso_objective::Weights>,
    notes: Vec<f64>,
    lower_base: f64,
}
pub(crate) struct Upper {
    pub base: f64,
    pub weights: Arc<crate::gekiso_objective::Weights>,
    pub(crate) id: usize,
    lower_base: f64,
    commands: usize,
    absolute: f64,
}
pub(crate) struct Choice<'a> {
    pub actor: &'a Performer,
    pub slot: &'a crate::live::Slot,
    pub index: usize,
    pub power: f64,
}
pub(crate) struct SourceRow<'a> {
    pub member: usize,
    pub snaps: &'a [i32],
    pub domain: u64,
    pub fixed: bool,
}
pub(crate) struct Assignment<'a> {
    pub actors: &'a [Option<Performer>],
    pub slots: &'a [crate::live::Slot],
    pub powers: &'a [f64],
    pub width: usize,
    pub rows: &'a [SourceRow<'a>; 5],
    pub used: &'a [bool],
    pub fixed_power: f64,
    pub criterion: crate::objectives::Criterion,
    pub cutoff: f64,
    pub base: &'a Arc<Upper>,
}
#[derive(Clone, Copy)]
struct Offer {
    snap: i32,
    power: f64,
    weight: f64,
}
fn witnesses(mut points: Vec<Offer>) -> Vec<Offer> {
    points.sort_unstable_by(|a, b| {
        b.power
            .total_cmp(&a.power)
            .then(b.weight.total_cmp(&a.weight))
            .then(a.snap.cmp(&b.snap))
    });
    let mut top = [f64::NEG_INFINITY; 5];
    points.retain(|point| {
        let keep = top[4] < point.weight;
        if point.weight > top[4] {
            let mut at = 4;
            while at > 0 && top[at - 1] < point.weight {
                top[at] = top[at - 1];
                at -= 1;
            }
            top[at] = point.weight;
        }
        keep
    });
    points.shrink_to_fit();
    points
}
struct Group {
    offers: [Vec<Offer>; 6],
    powers: Vec<(i32, f64)>,
    highs: [Vec<(i32, f64)>; 2],
    commands: usize,
    absolute: f64,
    normal_actions: usize,
    normal_absolute: f64,
}
struct NumericRow {
    events: [f64; 5],
    gain: f64,
    commands: usize,
    absolute: f64,
    normal_actions: usize,
    normal_absolute: f64,
}
#[derive(Default)]
pub(crate) struct Compiler {
    conversions: ConversionCache,
    traces: HashMap<Vec<usize>, (usize, Arc<Context>)>,
    effects: HashMap<(usize, usize), Arc<Effect>>,
    weights: HashMap<WeightKey, Arc<Upper>>,
    bases: HashMap<usize, Arc<Basis>>,
    numeric_rows: HashMap<(usize, usize, usize), Arc<NumericRow>>,
    groups: HashMap<(usize, usize, u64), Arc<Group>>,
    matching: crate::assignment::Scratch,
    matrix: Vec<Vec<f64>>,
    normal_ids: HashMap<usize, usize>,
    normal_programs: HashMap<Vec<u64>, usize>,
    modifiers: HashMap<usize, (Performer, usize)>,
    programs: HashMap<Performer, Program>,
    intern: HashMap<Program, Program>,
    bytes: usize,
    row_bytes: usize,
    next: usize,
    indices: HashMap<i32, usize>,
    baseline_gains: HashMap<Program, f64>,
    lower_weights: Vec<f64>,
}
impl Compiler {
    pub(crate) fn program_id(&mut self, master: &Master, actor: &Performer) -> usize {
        Arc::as_ptr(&self.program(master, actor)) as usize
    }
    pub(crate) fn baseline_gain_floor(
        &mut self,
        master: &Master,
        chart: &gekiso::Chart,
        better: &Performer,
        worse: &Performer,
    ) -> Result<f64, String> {
        if !self.program(master, worse).is_empty() {
            return Ok(0.0);
        }
        for &(id, level) in &better.gekisou_support_skills {
            for row in master.gekiso_support_skills[&id]
                .effects
                .iter()
                .filter(|r| r.level == level && r.kind != 4004)
            {
                if row.kind != 2001
                    || master.cumulative.get(&row.cumulative_id).is_none_or(|c| {
                        c.kind != 1000
                            || c.targets.is_empty()
                            || c.targets.iter().any(|id| master.targets[id].judgement != 6)
                    })
                {
                    return Ok(0.0);
                }
            }
        }
        let program = self.program(master, better);
        if program.is_empty() || program.first() == Some(&u64::MAX) {
            return Ok(0.0);
        }
        if let Some(&gain) = self.baseline_gains.get(&program) {
            return Ok(gain);
        }
        if self.lower_weights.is_empty() {
            self.lower_weights = chart
                .note_frac
                .iter()
                .enumerate()
                .map(|(i, &fraction)| {
                    let mut weight = master.live.adjustment;
                    for value in [
                        chart.difficulty_factor,
                        fraction,
                        chart.combo_factor[i],
                        chart.judge_frac[5].min(chart.judge_frac[6]),
                    ] {
                        weight = (weight * value).next_down().max(0.0);
                    }
                    (weight / chart.full.converted as f64).next_down().max(0.0)
                })
                .collect();
        }
        let mut model = Model::new(
            master,
            std::slice::from_ref(better),
            &chart.full.notes,
            &chart.full.events,
            chart.params(),
            Some(&chart.setup),
            (0, false),
        )?;
        for frame in chart.span.0..chart.span.1 {
            let input = &chart.play.frames[frame];
            model.frame(input.time_ms, &input.judged, chart.play.dt[frame])?;
        }
        let mut commands = model.score.score.note_factor_commands();
        commands.sort_by_key(|c| c.0);
        let (mut at, mut factor, mut gain) = (0, 0.0_f64, 0.0_f64);
        for (i, &time) in chart.times.iter().enumerate() {
            while at < commands.len() && commands[at].0 <= time {
                factor = (factor + f(f(commands[at].1 as f64) / 100000.0)).next_down();
                at += 1;
            }
            let term = (factor.max(0.0) * self.lower_weights[i])
                .next_down()
                .max(0.0);
            gain = (gain + term).next_down().max(0.0);
        }
        self.baseline_gains.insert(program, gain);
        Ok(gain)
    }
    pub(crate) fn score_dominates(
        &mut self,
        master: &Master,
        better: &Performer,
        worse: &Performer,
    ) -> bool {
        let split = |p: &Performer| {
            if p.gekisou_skill.is_some() {
                return None;
            }
            let mut convert = p.clone();
            let mut score = p.clone();
            convert.gekisou_support_skills.clear();
            score.gekisou_support_skills.clear();
            for &(id, level) in &p.gekisou_support_skills {
                let mut sc = false;
                let mut cv = false;
                for row in master.gekiso_support_skills[&id]
                    .effects
                    .iter()
                    .filter(|r| r.level == level)
                {
                    match row.kind {
                        2001 if row.value >= 0 && row.max_value > 0 => sc = true,
                        13005 => cv = true,
                        4004 => {}
                        _ => return None,
                    }
                }
                if sc && cv {
                    return None;
                }
                if sc {
                    score.gekisou_support_skills.push((id, level));
                }
                if cv {
                    convert.gekisou_support_skills.push((id, level));
                }
            }
            Some((convert, score))
        };
        let (Some((ac, ascore)), Some((bc, bscore))) = (split(better), split(worse)) else {
            return false;
        };
        let ac = self.program(master, &ac);
        let bc = self.program(master, &bc);
        if ac.first() == Some(&u64::MAX) || bc.first() == Some(&u64::MAX) || ac != bc {
            return false;
        }
        let a = self.program(master, &ascore);
        let b = self.program(master, &bscore);
        if a.first() == Some(&u64::MAX) || b.first() == Some(&u64::MAX) {
            return false;
        }
        if b.is_empty() {
            return true;
        }
        if a.len() != b.len() {
            return false;
        }
        let mut at = 0;
        while at < b.len() {
            if a[at + 2] != 2001 || b[at + 2] != 2001 {
                return false;
            }
            let end = at + 14 + b[at + 13] as usize;
            if a[at + 6] < b[at + 6] || a[at + 12] < b[at + 12] {
                return false;
            }
            for i in at..end {
                if i != at + 6 && i != at + 12 && a[i] != b[i] {
                    return false;
                }
            }
            at = end;
        }
        true
    }
    fn program(&mut self, master: &Master, p: &Performer) -> Arc<Vec<u64>> {
        if let Some(key) = self.programs.get(p) {
            return Arc::clone(key);
        }
        let mut key = Vec::new();
        let mut row_ids = HashMap::default();
        for &(id, level) in &p.gekisou_support_skills {
            let skill = &master.gekiso_support_skills[&id];
            for row in skill
                .effects
                .iter()
                .filter(|r| r.level == level && r.kind != 4004)
            {
                let static_condition = if row.condition_group == 0 {
                    Some(true)
                } else {
                    master
                        .condition_sets
                        .get(&row.condition_group)
                        .and_then(|sets| {
                            let mut any = false;
                            for set in sets {
                                let mut all = true;
                                for id in set {
                                    let c = &master.conditions[id];
                                    all &= match c.kind {
                                        5000 => {
                                            c.targets.iter().any(|id| {
                                                master.targets.get(id).is_some_and(|t| {
                                                    crate::full::conditions::performer_matches(p, t)
                                                })
                                            }) == c.positive
                                        }
                                        2000 => {
                                            (master.live.life_base <= *c.values.first()?)
                                                == c.positive
                                        }
                                        2001 => {
                                            (master.live.life_base >= *c.values.first()?)
                                                == c.positive
                                        }
                                        _ => return None,
                                    };
                                }
                                any |= all;
                            }
                            Some(any)
                        })
                };
                if static_condition == Some(false) {
                    continue;
                }
                let owner_dynamic = [row.trigger_group, row.release_group, row.reset_group]
                    .iter()
                    .flat_map(|g| master.condition_sets.get(g).into_iter().flatten().flatten())
                    .any(|id| {
                        master
                            .conditions
                            .get(id)
                            .is_some_and(|c| [5000, 2000, 2001].contains(&c.kind))
                    });
                if static_condition.is_none() || owner_dynamic {
                    key = serde_json::to_vec(p)
                        .unwrap()
                        .into_iter()
                        .map(|c| c as u64)
                        .collect();
                    key.insert(0, u64::MAX);
                    let key = Arc::new(key);
                    self.programs.insert(p.clone(), Arc::clone(&key));
                    return key;
                }
                let next = row_ids.len() as u64;
                key.push(*row_ids.entry(row.id).or_insert(next));
                key.extend([
                    skill.mission_type as u64,
                    row.kind as u64,
                    row.trigger_type as u64,
                    row.execute_limit as u64,
                    row.reset_group as u64,
                    row.value as u64,
                    row.seconds.to_bits(),
                    row.trigger_group as u64,
                    row.release_group as u64,
                    row.cumulative_id as u64,
                    row.limit_count as u64,
                    row.max_value as u64,
                    row.targets.len() as u64,
                ]);
                key.extend(row.targets.iter().map(|&v| v as u64));
            }
        }
        let key = Arc::new(key);
        let key = Arc::clone(self.intern.entry(Arc::clone(&key)).or_insert(key));
        self.programs.insert(p.clone(), Arc::clone(&key));
        key
    }
    pub(crate) fn upper(
        &mut self,
        master: &Master,
        chart: &gekiso::Chart,
        deck: &[Performer],
    ) -> Result<Option<Arc<Upper>>, String> {
        let mut converters = Vec::new();
        let mut modifiers = Vec::new();
        for actor in deck {
            if actor.gekisou_skill.is_some() {
                return Ok(None);
            }
            let mut converter = actor.clone();
            let mut modifier = actor.clone();
            converter.gekisou_support_skills.clear();
            modifier.gekisou_support_skills.clear();
            for &(id, level) in &actor.gekisou_support_skills {
                let skill = &master.gekiso_support_skills[&id];
                let mut score = false;
                let mut convert = false;
                for row in skill.effects.iter().filter(|r| r.level == level) {
                    match row.kind {
                        2001 if row.value >= 0 && row.max_value >= 0 => score = true,
                        13005 => convert = true,
                        4004 => {}
                        _ => return Ok(None),
                    }
                }
                if score && convert {
                    return Ok(None);
                }
                if score {
                    modifier.gekisou_support_skills.push((id, level));
                }
                if convert {
                    converter.gekisou_support_skills.push((id, level));
                }
            }
            if !converter.gekisou_support_skills.is_empty() {
                for &(id, level) in &converter.gekisou_support_skills {
                    for row in master.gekiso_support_skills[&id]
                        .effects
                        .iter()
                        .filter(|r| r.level == level && r.kind == 13005)
                    {
                        if row.cumulative_id != 0
                            || [row.trigger_group, row.release_group, row.reset_group]
                                .iter()
                                .flat_map(|g| {
                                    master.condition_sets.get(g).into_iter().flatten().flatten()
                                })
                                .any(|id| {
                                    ![0, 1030, 7010, 7013, 7020]
                                        .contains(&master.conditions[id].kind)
                                })
                        {
                            return Ok(None);
                        }
                    }
                }
                converters.push(converter);
            }
            modifiers.push(modifier);
        }
        if self.bytes > 16 * 1024 * 1024 {
            self.conversions.clear();
            self.traces.clear();
            self.effects.clear();
            self.weights.clear();
            self.bases.clear();
            self.bytes = 0;
        }
        let conversion_key: Vec<_> = converters
            .iter()
            .map(|p| Arc::as_ptr(&self.program(master, p)) as usize)
            .collect();
        let mut modifier_keys: Vec<_> = modifiers
            .iter()
            .map(|p| Arc::as_ptr(&self.program(master, p)) as usize)
            .collect();
        modifier_keys.sort_unstable();
        let (id, converted) = if let Some(value) = self.conversions.get(&conversion_key) {
            (value.0, Arc::clone(&value.1))
        } else {
            let context = Arc::new(gekiso::context_fixed(master, chart, &converters, &[0])?);
            if context.up.iter().any(|&v| v != 1.0)
                || context.gk.iter().any(|&v| v != 1.0)
                || context.luck.iter().any(|&v| v != 1.0)
            {
                return Ok(None);
            }
            let (id, context) = if let Some((id, previous)) = self.traces.get(&context.judgement) {
                (*id, Arc::clone(previous))
            } else {
                self.next += 1;
                self.bytes += context.retained_bytes()
                    + context.judgement.capacity() * std::mem::size_of::<usize>();
                self.traces
                    .insert(context.judgement.clone(), (self.next, Arc::clone(&context)));
                (self.next, context)
            };
            self.conversions
                .insert(conversion_key, (id, Arc::clone(&context)));
            (id, context)
        };
        let weight_key = (id, modifier_keys);
        if let Some(weights) = self.weights.get(&weight_key) {
            return Ok(Some(Arc::clone(weights)));
        }
        if self.indices.is_empty() {
            self.indices = chart
                .notes
                .iter()
                .enumerate()
                .map(|(i, n)| (n.id, i))
                .collect();
        }
        let mut effects = Vec::new();
        for actor in modifiers {
            if actor.gekisou_support_skills.is_empty() {
                continue;
            }
            let key = (id, Arc::as_ptr(&self.program(master, &actor)) as usize);
            let effect = if let Some(effect) = self.effects.get(&key) {
                Arc::clone(effect)
            } else {
                let mut model = Model::new(
                    master,
                    &[actor],
                    &chart.full.notes,
                    &chart.full.events,
                    chart.params(),
                    Some(&chart.setup),
                    (0, false),
                )?;
                for k in chart.span.0..chart.span.1 {
                    let frame = &chart.play.frames[k];
                    let mut judged = frame.judged.clone();
                    for note in &mut judged {
                        let Some(&i) = self.indices.get(&note.note_id) else {
                            return Ok(None);
                        };
                        note.judgement = converted.judgement[i] as i32;
                    }
                    model.frame(frame.time_ms, &judged, chart.play.dt[k])?;
                }
                let mut commands = model.score.score.note_factor_commands();
                commands.sort_by_key(|c| c.0);
                let absolute = commands.iter().fold(0.0_f64, |sum, c| {
                    (sum + f(f(c.1 as f64) / 100000.0).abs()).next_up()
                });
                let (mut at, mut sum) = (0, 0.0_f64);
                let values = chart
                    .times
                    .iter()
                    .map(|&time| {
                        while at < commands.len() && commands[at].0 <= time {
                            sum = (sum + f(f(commands[at].1 as f64) / 100000.0)).next_up();
                            at += 1;
                        }
                        sum
                    })
                    .collect();
                let effect = Arc::new(Effect {
                    values,
                    absolute,
                    commands: commands.len(),
                });
                self.bytes += effect.values.len() * 8 + 128;
                self.effects.insert(key, Arc::clone(&effect));
                effect
            };
            effects.push(effect);
        }
        let Some(ranges) = crate::rewards::deterministic_ranges(master, chart) else {
            return Ok(None);
        };
        let operations = crate::rewards::factor_operations(chart, &ranges) as f64
            * effects.iter().map(|e| e.commands).sum::<usize>() as f64;
        let nu = (operations * 2.0_f64.powi(-24)).next_up();
        if nu >= 0.25 {
            return Ok(None);
        }
        let absolute = effects
            .iter()
            .fold(0.0_f64, |sum, e| (sum + e.absolute).next_up());
        let error = (4.0 * nu * absolute / (1.0 - nu)).next_up();
        let basis = if let Some(basis) = self.bases.get(&id) {
            Arc::clone(basis)
        } else {
            let weights = Arc::new(crate::gekiso_objective::note_weights(
                master, chart, &converted,
            ));
            let combo = chart.combo(master, &converted.judgement);
            let unit = master.live.adjustment * chart.difficulty_factor;
            let mut lower_base = 0.0_f64;
            let mut notes = Vec::with_capacity(chart.notes.len());
            for i in 0..chart.notes.len() {
                let range = chart.range_of[i];
                let range_factor = if range >= 0 {
                    1.0 + chart.range_percent[range as usize] as f64 / 100.0
                } else {
                    1.0
                };
                notes.push(
                    range_factor
                        * unit
                        * chart.note_frac[i]
                        * chart.judge_frac[converted.judgement[i]]
                        * combo[i]
                        * converted.gk[i],
                );
                let mut beta = 1.0_f64;
                for value in [
                    range_factor,
                    master.live.adjustment,
                    chart.difficulty_factor,
                    chart.note_frac[i],
                    chart.judge_frac[converted.judgement[i]],
                    combo[i],
                    converted.gk[i],
                ] {
                    beta = (beta * value).next_down().max(0.0);
                }
                lower_base = (lower_base + beta).next_down().max(0.0);
            }
            self.bytes += 256
                + notes.capacity() * 8
                + weights.general.capacity() * 8
                + weights
                    .judgements
                    .iter()
                    .map(|row| row.capacity() * 8)
                    .sum::<usize>();
            let basis = Arc::new(Basis {
                weights,
                notes,
                lower_base,
            });
            self.bases.insert(id, Arc::clone(&basis));
            basis
        };
        let mut base = 0.0_f64;
        for (i, &beta) in basis.notes.iter().enumerate() {
            let sum = effects
                .iter()
                .fold(1.0_f64, |sum, effect| (sum + effect.values[i]).next_up());
            base += beta * (sum + error).next_up().max(0.0);
        }
        self.bytes += 256 + 32 * (1 + weight_key.1.len());
        let upper = Arc::new(Upper {
            base,
            weights: Arc::clone(&basis.weights),
            id,
            lower_base: basis.lower_base,
            commands: effects.iter().map(|e| e.commands).sum(),
            absolute,
        });
        self.weights.insert(weight_key, Arc::clone(&upper));
        Ok(Some(upper))
    }
    fn numeric_row(
        &mut self,
        master: &Master,
        chart: &gekiso::Chart,
        converters: &[Performer],
        base: &Arc<Upper>,
        choice: &Choice<'_>,
    ) -> Result<Option<Arc<NumericRow>>, String> {
        if choice.actor.gekisou_skill.is_some() || choice.slot.effects.iter().any(|e| e.delta < 0) {
            return Ok(None);
        }
        let normal_id = if let Some(&id) = self.normal_ids.get(&choice.index) {
            id
        } else {
            let signature = choice.slot.signature();
            let next = self.normal_programs.len();
            let id = *self.normal_programs.entry(signature).or_insert(next);
            self.normal_ids.insert(choice.index, id);
            id
        };
        if !self.modifiers.contains_key(&choice.index) {
            let mut actor = choice.actor.clone();
            let mut skills = Vec::new();
            for &(id, level) in &actor.gekisou_support_skills {
                let rows: Vec<_> = master.gekiso_support_skills[&id]
                    .effects
                    .iter()
                    .filter(|r| r.level == level)
                    .collect();
                let score = rows.iter().any(|r| r.kind == 2001);
                let convert = rows.iter().any(|r| r.kind == 13005);
                if score && convert || rows.iter().any(|r| ![2001, 13005, 4004].contains(&r.kind)) {
                    return Ok(None);
                }
                if score {
                    skills.push((id, level));
                }
            }
            actor.gekisou_support_skills = skills;
            let id = Arc::as_ptr(&self.program(master, &actor)) as usize;
            self.modifiers.insert(choice.index, (actor, id));
        }
        let (modifier, modifier_id) = self.modifiers[&choice.index].clone();
        let key = (base.id, normal_id, modifier_id);
        if let Some(row) = self.numeric_rows.get(&key) {
            return Ok(Some(Arc::clone(row)));
        }
        let (gain, commands, absolute) = if modifier.gekisou_support_skills.is_empty() {
            (0.0, 0, 0.0)
        } else {
            let mut deck = converters.to_vec();
            deck.push(modifier);
            let Some(single) = self.upper(master, chart, &deck)? else {
                return Ok(None);
            };
            let nu = (16.0 * (chart.notes.len() + 1) as f64 * f64::EPSILON).next_up();
            if nu >= 0.25 {
                return Ok(None);
            }
            let high = (single.base * (1.0 / (1.0 - nu)).next_up()).next_up();
            (
                (high - base.lower_base).next_up().max(0.0),
                single.commands,
                single.absolute,
            )
        };
        let mut events = [0.0; 5];
        for effect in &choice.slot.effects {
            if effect.gate.blocks(master.live.life_base)
                && effect.gate.blocks(2.0 * master.live.life_base)
            {
                continue;
            }
            let diff = f(effect.delta as f64 / 100000.0);
            let duration = crate::num::window_ms(effect.seconds, choice.slot.extension_ms);
            for (event, &(_, time)) in chart.full.events.iter().take(5).enumerate() {
                let start = chart.times.partition_point(|&t| t < time);
                let end = chart.times.partition_point(|&t| t < time + duration);
                let weight = if effect.kind == 2000 {
                    base.weights.general[end] - base.weights.general[start]
                } else {
                    effect
                        .judgements
                        .iter()
                        .filter(|&&j| (4..=6).contains(&j))
                        .map(|&j| {
                            base.weights.judgements[j - 4][end]
                                - base.weights.judgements[j - 4][start]
                        })
                        .sum()
                };
                events[event] = (events[event] + (diff * weight).next_up()).next_up();
            }
        }
        let (normal_actions, normal_absolute) = crate::live::rounding_profile(choice.slot, None);
        let row = Arc::new(NumericRow {
            events,
            gain,
            commands,
            absolute,
            normal_actions,
            normal_absolute,
        });
        self.numeric_rows.insert(key, Arc::clone(&row));
        self.row_bytes += 256;
        Ok(Some(row))
    }
    fn group(
        &mut self,
        master: &Master,
        chart: &gekiso::Chart,
        converters: &[Performer],
        base: &Arc<Upper>,
        input: &Assignment<'_>,
        row: &SourceRow<'_>,
    ) -> Result<Option<Arc<Group>>, String> {
        let key = (base.id, row.member, row.domain);
        if let Some(group) = self.groups.get(&key) {
            return Ok(Some(Arc::clone(group)));
        }
        let mut group = Group {
            offers: std::array::from_fn(|_| Vec::new()),
            powers: Vec::new(),
            highs: std::array::from_fn(|_| Vec::new()),
            commands: 0,
            absolute: 0.0,
            normal_actions: 0,
            normal_absolute: 0.0,
        };
        for &snap in row.snaps {
            let index = row.member * input.width + (snap + 1) as usize;
            let choice = Choice {
                actor: input.actors[index].as_ref().unwrap(),
                slot: &input.slots[index],
                index,
                power: input.powers[row.member * input.width
                    + if snap < 0 {
                        input.width - 1
                    } else {
                        snap as usize
                    }],
            };
            let Some(profile) = self.numeric_row(master, chart, converters, base, &choice)? else {
                return Ok(None);
            };
            group.commands = group.commands.max(profile.commands);
            group.absolute = group.absolute.max(profile.absolute);
            group.normal_actions = group.normal_actions.max(profile.normal_actions);
            group.normal_absolute = group.normal_absolute.max(profile.normal_absolute);
            group.powers.push((snap, choice.power));
            let mean = (profile
                .events
                .iter()
                .fold(0.0_f64, |sum, &v| (sum + v).next_up())
                / 5.0)
                .next_up();
            group.highs[0].push((snap, (profile.gain + mean).next_up()));
            group.highs[1].push((
                snap,
                (profile.gain + profile.events.iter().copied().fold(0.0, f64::max)).next_up(),
            ));
            for event in 0..5 {
                group.offers[event].push(Offer {
                    snap,
                    power: choice.power,
                    weight: (profile.gain + profile.events[event]).next_up(),
                });
            }
            group.offers[5].push(Offer {
                snap,
                power: choice.power,
                weight: (profile.gain + mean).next_up(),
            });
        }
        group.powers.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        for values in &mut group.highs {
            values.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        }
        group.powers.truncate(5);
        group.powers.shrink_to_fit();
        for values in &mut group.highs {
            values.truncate(5);
            values.shrink_to_fit();
        }
        group.offers = group.offers.map(witnesses);
        let bytes = 512
            + group.offers.iter().map(Vec::capacity).sum::<usize>() * std::mem::size_of::<Offer>()
            + (group.powers.capacity() + group.highs.iter().map(Vec::capacity).sum::<usize>())
                * std::mem::size_of::<(i32, f64)>();
        let group = Arc::new(group);
        self.groups.insert(key, Arc::clone(&group));
        self.row_bytes += bytes;
        Ok(Some(group))
    }
    pub(crate) fn assignment_upper(
        &mut self,
        master: &Master,
        chart: &gekiso::Chart,
        converters: &[Performer],
        input: &Assignment<'_>,
    ) -> Result<Option<f64>, String> {
        use crate::objectives::Criterion;
        if self.row_bytes > 16 * 1024 * 1024 {
            self.numeric_rows.clear();
            self.groups.clear();
            self.row_bytes = 0;
        }
        if input.rows.iter().any(|row| row.snaps.is_empty()) {
            return Ok(Some(f64::NEG_INFINITY));
        }
        let base = input.base;
        let (mut commands, mut absolute, mut normal_actions, mut normal_absolute) =
            (0usize, 0.0_f64, 0usize, 0.0_f64);
        let mut groups = Vec::with_capacity(5);
        let mut power_cap = input.fixed_power;
        let mut variable_cap = 0.0_f64;
        let available = |snap: i32, fixed: bool| fixed || snap < 0 || !input.used[snap as usize];
        for row in input.rows {
            let Some(group) = self.group(master, chart, converters, base, input, row)? else {
                return Ok(None);
            };
            let Some(&(_, power)) = group
                .powers
                .iter()
                .find(|&&(snap, _)| available(snap, row.fixed))
            else {
                return Ok(Some(f64::NEG_INFINITY));
            };
            let Some(&(_, high)) = group.highs[(input.criterion != Criterion::Mean) as usize]
                .iter()
                .find(|&&(snap, _)| available(snap, row.fixed))
            else {
                return Ok(Some(f64::NEG_INFINITY));
            };
            commands += group.commands;
            absolute = (absolute + group.absolute).next_up();
            normal_actions += group.normal_actions;
            normal_absolute = (normal_absolute + group.normal_absolute).next_up();
            power_cap = (power_cap + power).next_up();
            variable_cap = (variable_cap + high).next_up();
            groups.push(group);
        }
        let Some(ranges) = crate::rewards::deterministic_ranges(master, chart) else {
            return Ok(None);
        };
        let u = 2.0_f64.powi(-24);
        let nu = (crate::rewards::factor_operations(chart, &ranges) as f64 * commands as f64 * u)
            .next_up();
        let normal_nu = ((2 * normal_actions + 5) as f64 * u).next_up();
        if nu >= 0.25 || normal_nu >= 0.25 {
            return Ok(None);
        }
        let gk_error = (4.0 * nu * absolute / (1.0 - nu)).next_up();
        let normal_error = (16.0 * normal_nu * normal_absolute / (1.0 - normal_nu)).next_up();
        let total_weight = base.weights.general[chart.notes.len()];
        let constant =
            (base.base + ((gk_error + normal_error).next_up() * total_weight).next_up()).next_up();
        let coefficient = (constant + variable_cap).next_up();
        let lambda = (coefficient / power_cap.max(1.0)).sqrt();
        if !lambda.is_finite() || lambda <= 0.0 {
            return Ok(None);
        }
        let inverse = (1.0 / lambda).next_up();
        let mut offers = [[f64::NEG_INFINITY; 5]; 5];
        for slot in 0..5 {
            let count = if input.criterion == Criterion::Mean {
                1
            } else {
                5
            };
            for (event, target) in offers[slot].iter_mut().enumerate().take(count) {
                let channel = if input.criterion == Criterion::Mean {
                    5
                } else {
                    event
                };
                for point in &groups[slot].offers[channel] {
                    if !available(point.snap, input.rows[slot].fixed) {
                        continue;
                    }
                    let value = ((lambda * point.power).next_up()
                        + (point.weight * inverse).next_up())
                    .next_up();
                    *target = target.max(value);
                }
            }
        }
        let mut previous = [0usize; 32];
        let variable = if input.criterion == Criterion::Mean {
            offers
                .iter()
                .fold(0.0_f64, |sum, row| (sum + row[0]).next_up())
        } else {
            let mut dp = [if input.criterion == Criterion::Min {
                f64::INFINITY
            } else {
                f64::NEG_INFINITY
            }; 32];
            dp[0] = 0.0;
            for mask in 0usize..31 {
                let slot = mask.count_ones() as usize;
                for (event, &weight) in offers[slot].iter().enumerate() {
                    if mask & (1 << event) == 0 {
                        let next = mask | (1 << event);
                        let value = (dp[mask] + weight).next_up();
                        if input.criterion == Criterion::Min {
                            if value < dp[next] {
                                dp[next] = value;
                                previous[next] = event;
                            }
                        } else {
                            dp[next] = dp[next].max(value);
                        }
                    }
                }
            }
            dp[31]
        };
        let total = (((lambda * input.fixed_power).next_up() + (constant * inverse).next_up())
            .next_up()
            + variable)
            .next_up();
        let mut rounding = 1.0_f64;
        for _ in 0..12 {
            rounding = (rounding * (1.0 / (1.0 - u)).next_up()).next_up();
        }
        let score =
            (((total * total).next_up() / 4.0).next_up() / chart.full.converted as f64).next_up();
        let mut upper = (score * rounding).next_up() + (chart.notes.len() + 4) as f64;
        if upper >= input.cutoff {
            let mut order = [0usize; 5];
            if input.criterion == Criterion::Min {
                let mut mask = 31usize;
                for slot in (0..5).rev() {
                    order[slot] = previous[mask];
                    mask ^= 1 << order[slot];
                }
            }
            let columns = input.used.len() + 5;
            let bad = -1e100;
            if self.matrix.is_empty() {
                self.matrix = vec![vec![bad; columns]; 5];
            }
            for row in &mut self.matrix {
                row.fill(bad);
            }
            for slot in 0..5 {
                let channels: &[usize] = match input.criterion {
                    Criterion::Mean => &[5],
                    Criterion::Min => &order[slot..slot + 1],
                    Criterion::Max => &[0, 1, 2, 3, 4],
                };
                for &channel in channels {
                    for point in &groups[slot].offers[channel] {
                        if !available(point.snap, input.rows[slot].fixed) {
                            continue;
                        }
                        let value = ((lambda * point.power).next_up()
                            + (point.weight * inverse).next_up())
                        .next_up();
                        if point.snap < 0 {
                            for cell in &mut self.matrix[slot][input.used.len()..] {
                                *cell = cell.max(value);
                            }
                        } else {
                            let cell = &mut self.matrix[slot][point.snap as usize];
                            *cell = cell.max(value);
                        }
                    }
                }
            }
            let matched = self.matching.maximum_upper(&self.matrix, columns);
            if matched < bad / 2.0 {
                return Ok(Some(f64::NEG_INFINITY));
            }
            let total = (((lambda * input.fixed_power).next_up() + (constant * inverse).next_up())
                .next_up()
                + matched)
                .next_up();
            let bound = (((total * total).next_up() / 4.0).next_up() / chart.full.converted as f64)
                .next_up();
            upper = upper.min((bound * rounding).next_up() + (chart.notes.len() + 4) as f64);
        }
        Ok(Some(upper))
    }
}
