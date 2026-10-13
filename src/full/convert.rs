//! Most-recent-first judgement conversion and activation limits.
use std::collections::{HashMap, HashSet};
#[derive(Clone)]
pub struct Effect {
    pub kind: i32,
    pub value: i32,
    pub limit: i32,
    pub targets: Vec<i32>,
    pub id: i32,
}
#[derive(Clone)]
struct Parameter {
    key: String,
    count: i32,
    maximum: i32,
    targets: Vec<i32>,
    to: i32,
}
#[derive(Clone)]
pub struct Conversion {
    no_just: HashSet<i32>,
    order: Vec<i32>,
    registered: HashSet<i32>,
    function_id: i32,
    ids: HashMap<String, i32>,
    parameters: HashMap<i32, Parameter>,
    finished: HashMap<String, i32>,
    context: HashMap<i32, Vec<i32>>,
    pub converted: i32,
}
impl Conversion {
    pub fn new(no_just: HashSet<i32>) -> Self {
        Self {
            no_just,
            order: Vec::new(),
            registered: HashSet::new(),
            function_id: 0,
            ids: HashMap::new(),
            parameters: HashMap::new(),
            finished: HashMap::new(),
            context: HashMap::new(),
            converted: 0,
        }
    }
    fn register(&mut self) -> i32 {
        self.function_id += 1;
        let id = self.function_id;
        self.registered.insert(id);
        self.order.insert(0, id);
        id
    }
    fn unregister(&mut self, id: i32) -> Result<(), String> {
        if !self.registered.remove(&id) {
            return Err(format!("judgement convert function {id} is not registered"));
        }
        if let Some(index) = self.order.iter().position(|&i| i == id) {
            self.order.remove(index);
        }
        Ok(())
    }
    pub fn convert(
        &mut self,
        judgement: i32,
        judgement_type: i32,
        time: i32,
    ) -> Result<i32, String> {
        let mut i = 0;
        while i < self.order.len() {
            let id = self.order[i];
            if !self.registered.contains(&id) {
                i += 1;
                continue;
            }
            let out = self.try_convert(id, judgement, judgement_type, time)?;
            if out != judgement {
                return Ok(out);
            }
            if i < self.order.len() && self.order[i] == id {
                i += 1;
            }
        }
        Ok(judgement)
    }
    fn try_convert(
        &mut self,
        id: i32,
        current: i32,
        judgement_type: i32,
        time: i32,
    ) -> Result<i32, String> {
        let Some(parameter) = self.parameters.get_mut(&id) else {
            return Ok(current);
        };
        if parameter.to == -1
            || !parameter.targets.contains(&current)
            || current == parameter.to
            || (parameter.to == 6 && self.no_just.contains(&judgement_type))
        {
            return Ok(current);
        }
        parameter.count += 1;
        let to = parameter.to;
        let key = parameter.key.clone();
        let complete = parameter.maximum > 0 && parameter.maximum <= parameter.count;
        if complete {
            self.unregister(id)?;
            self.ids.remove(&key);
            self.parameters.remove(&id);
            self.finished.insert(key, time);
        }
        self.converted += 1;
        Ok(to)
    }
    pub fn update(
        &mut self,
        key: &str,
        mut state: i32,
        effect: &Effect,
    ) -> Result<Option<i32>, String> {
        let finished = self.finished.remove(key);
        if finished.is_some() {
            state = 4;
        }
        if state == 4 {
            if let Some(id) = self.ids.remove(key) {
                self.unregister(id)?;
                self.parameters.remove(&id);
            }
        } else if state == 2 {
            let targets = self
                .context
                .entry(effect.id)
                .or_insert_with(|| effect.targets.clone())
                .clone();
            let to = if effect.kind == 13005 {
                6
            } else if (0..=5).contains(&(effect.value - 1)) {
                effect.value
            } else {
                -1
            };
            let id = self.register();
            if self.ids.contains_key(key) {
                return Err("judgement convert state registered twice".into());
            }
            self.ids.insert(key.into(), id);
            self.parameters.insert(
                id,
                Parameter {
                    key: key.into(),
                    count: 0,
                    maximum: effect.limit,
                    targets,
                    to,
                },
            );
        }
        Ok(finished)
    }
}
