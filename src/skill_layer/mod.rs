pub mod environment;
pub mod lazy;
pub mod loader;
pub mod metadata_cache;
pub mod process_group;
pub mod rpc_client;

tokio::task_local! {
    pub static CALLER: String;
}

pub fn current_caller() -> Option<String> {
    CALLER.try_with(|id| id.clone()).ok()
}

use std::collections::HashMap;

use jumabek_sdk::{ModuleMetadata, SkillModule};

pub struct SkillRegistry {
    skills: HashMap<String, Box<dyn SkillModule>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        SkillRegistry {
            skills: HashMap::new(),
        }
    }

    pub fn register(&mut self, skill: Box<dyn SkillModule>) {
        let name = skill.get_metadata().name.clone();
        self.skills.insert(name, skill);
    }

    pub fn get(&self, name: &str) -> Option<&dyn SkillModule> {
        self.skills.get(name).map(|b| b.as_ref())
    }

    pub fn list(&self) -> Vec<&ModuleMetadata> {
        self.skills.values().map(|s| s.get_metadata()).collect()
    }

    pub fn all(&self) -> impl Iterator<Item = &dyn SkillModule> {
        self.skills.values().map(|s| s.as_ref())
    }
}
