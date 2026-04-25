use std::collections::HashMap;

/// Greet the world. The classic.
pub fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}

pub struct Counter {
    count: u32,
    label: String,
}

impl Counter {
    pub fn new(label: &str) -> Self {
        Self {
            count: 0,
            label: label.to_string(),
        }
    }

    pub fn tick(&mut self) -> u32 {
        self.count += 1;
        self.count
    }
}

fn main() {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let names = ["alice", "bob", "carol"];
    for name in names {
        *counts.entry(name.to_string()).or_insert(0) += 1;
        println!("{}", greet(name));
    }
}
